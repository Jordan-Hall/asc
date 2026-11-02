//! U50 acceptance: the CLI against an in-process `asc serve`: clone, commit, push and fetch
//! between two clients; git's rejection for a plain push; `push --land` with its rewrite;
//! discovery with an `objects` tier; and `asc auth status`. The server is the real handler stack
//! over an in-memory database; the clients are the built `asc` binary.
#![cfg(feature = "serve")]

mod server_fixture;
mod support;

use std::path::Path;
use std::process::Output;

use server_fixture::{READ, Server, WRITE, WellKnown};
use support::{Sandbox, text};

const IDENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

fn asc_as(sb: &Sandbox, cwd: &Path, token: Option<&str>, args: &[&str]) -> Output {
    let mut cmd = sb.asc(cwd);
    cmd.envs(IDENT).args(args);
    if let Some(t) = token {
        cmd.env("ASC_TOKEN", t);
    }
    cmd.output().expect("spawn asc")
}

fn asc(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Output {
    asc_as(sb, cwd, Some(WRITE), args)
}

fn ok(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Output {
    let out = asc(sb, cwd, args);
    assert!(
        out.status.success(),
        "asc {args:?} failed: {}",
        text(&out.stderr)
    );
    out
}

fn commit_file(sb: &Sandbox, repo: &Path, name: &str, body: &str) {
    std::fs::write(repo.join(name), body).unwrap();
    ok(sb, repo, &["add", name]);
    ok(sb, repo, &["commit", "-q", "-m", name]);
}

fn head(sb: &Sandbox, repo: &Path, rev: &str) -> String {
    text(&ok(sb, repo, &["rev-parse", rev]).stdout)
        .trim()
        .to_owned()
}

/// A client with `origin` at `url`, one commit (`one.txt`) pushed to `master`.
fn first_client(sb: &Sandbox, name: &str, url: &str) -> std::path::PathBuf {
    let a = sb.dir(name);
    ok(sb, &a, &["init", "-q"]);
    ok(sb, &a, &["remote", "add", "origin", url]);
    commit_file(sb, &a, "one.txt", "one\n");
    let out = ok(sb, &a, &["push", "origin", "master"]);
    let err = text(&out.stderr);
    assert!(err.contains("[new branch]"), "{err}");
    assert!(err.contains("master -> master"), "{err}");
    a
}

#[test]
fn clone_commit_push_fetch_between_two_clients() {
    let server = Server::start(&["acme/app"]);
    let url = server.url("acme/app");
    let sb = Sandbox::new();
    let a = first_client(&sb, "a", &url);

    let root = sb.tmp.path().to_owned();
    ok(&sb, &root, &["clone", "-q", &url, "b"]);
    let b = root.join("b");
    assert_eq!(std::fs::read_to_string(b.join("one.txt")).unwrap(), "one\n");
    assert_eq!(head(&sb, &a, "HEAD"), head(&sb, &b, "HEAD"));
    assert_eq!(head(&sb, &b, "origin/master"), head(&sb, &b, "HEAD"));

    // A second commit travels the other way: pushed by `a`, fetched by `b`.
    commit_file(&sb, &a, "two.txt", "two\n");
    ok(&sb, &a, &["push", "origin", "master"]);
    let fetched = ok(&sb, &b, &["fetch", "origin"]);
    assert!(
        text(&fetched.stderr).contains("master"),
        "{}",
        text(&fetched.stderr)
    );
    assert_eq!(head(&sb, &b, "origin/master"), head(&sb, &a, "HEAD"));
    let log = text(&ok(&sb, &b, &["log", "--oneline", "origin/master"]).stdout);
    assert_eq!(log.lines().count(), 2, "{log}");

    // Files come on demand through the server's object tier.
    let shown = text(&ok(&sb, &b, &["show", "origin/master:two.txt"]).stdout);
    assert_eq!(shown, "two\n");
}

#[test]
fn a_clone_on_a_fresh_machine_reads_everything_through_the_server() {
    let server = Server::start(&["acme/app"]);
    let url = server.url("acme/app");
    let sb = Sandbox::new();
    let _a = first_client(&sb, "a", &url);
    let other = Sandbox::new();
    let root = other.tmp.path().to_owned();
    let out = asc(&other, &root, &["clone", "-q", &url, "c"]);
    assert!(
        out.status.success(),
        "{} {:?}",
        text(&out.stderr),
        server.calls()
    );
    let c = root.join("c");
    assert_eq!(
        std::fs::read_to_string(c.join("one.txt")).unwrap(),
        "one
"
    );
    assert!(server.count("GetObjects") > 0);
}

#[test]
fn a_clone_shows_a_folder_path_rules_keep_from_the_caller_as_restricted() {
    let server = Server::start(&["acme/mono"]);
    let url = server.url("acme/mono");
    let sb = Sandbox::new();
    let a = first_client(&sb, "a", &url);
    std::fs::create_dir(a.join("secret")).unwrap();
    commit_file(
        &sb,
        &a,
        "secret/key.txt",
        "hidden
",
    );
    ok(&sb, &a, &["push", "origin", "master"]);
    server.set_path_rules("acme/mono", &[("secret", "*", "none")]);

    let other = Sandbox::new();
    let root = other.tmp.path().to_owned();
    server.forget_calls();
    let out = asc_as(&other, &root, Some(READ), &["clone", "-q", &url, "c"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let c = root.join("c");
    assert_eq!(
        std::fs::read_to_string(c.join("one.txt")).unwrap(),
        "one
"
    );
    // The folder is there, holding nothing of the content, and its names do not leak.
    let names: Vec<_> = std::fs::read_dir(c.join("secret"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, [".asc-composed"]);
    assert!(!c.join("secret/key.txt").exists());

    let st = asc_as(&other, &c, Some(READ), &["status", "--porcelain"]);
    assert!(st.status.success(), "{}", text(&st.stderr));
    assert_eq!(text(&st.stdout), "", "a restricted folder is not a change");
    assert!(
        text(&st.stderr).contains("secret: restricted"),
        "{}",
        text(&st.stderr)
    );
    // History still reads, with the restricted change's message withheld.
    let log = text(&asc_as(&other, &c, Some(READ), &["log", "--format=%s"]).stdout);
    assert_eq!(log.lines().count(), 2, "{log}");
    assert!(!log.contains("secret/key.txt"), "{log}");
}

#[test]
fn a_plain_push_while_behind_is_git_s_rejection() {
    let server = Server::start(&["acme/app"]);
    let url = server.url("acme/app");
    let sb = Sandbox::new();
    let a = first_client(&sb, "a", &url);
    let root = sb.tmp.path().to_owned();
    ok(&sb, &root, &["clone", "-q", &url, "b"]);
    let b = root.join("b");

    commit_file(&sb, &a, "from-a.txt", "a\n");
    ok(&sb, &a, &["push", "origin", "master"]);
    commit_file(&sb, &b, "from-b.txt", "b\n");
    let out = asc(&sb, &b, &["push", "origin", "master"]);
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(
        err.contains("! [rejected]") && err.contains("(fetch first)"),
        "{err}"
    );
    assert!(err.contains("error: failed to push some refs"), "{err}");

    // After a fetch the same push is a non-fast-forward, as in git.
    ok(&sb, &b, &["fetch", "origin"]);
    let out = asc(&sb, &b, &["push", "origin", "master"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("(non-fast-forward)"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn push_land_rebases_on_the_server_and_moves_local_refs() {
    let server = Server::start(&["acme/app"]);
    let url = server.url("acme/app");
    let sb = Sandbox::new();
    let a = first_client(&sb, "a", &url);
    let root = sb.tmp.path().to_owned();
    ok(&sb, &root, &["clone", "-q", &url, "b"]);
    let b = root.join("b");

    // `a` moves the branch; `b` commits a different file on the old tip.
    commit_file(&sb, &a, "from-a.txt", "a\n");
    ok(&sb, &a, &["push", "origin", "master"]);
    commit_file(&sb, &b, "from-b.txt", "b\n");
    let before = head(&sb, &b, "HEAD");
    assert_eq!(
        text(&ok(&sb, &b, &["status", "--porcelain"]).stdout),
        "",
        "clean before the land"
    );

    let out = asc(&sb, &b, &["push", "--land", "origin", "master"]);
    let err = text(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("(landed: rebased onto"), "{err}");
    assert!(err.contains("master -> master"), "{err}");

    // The local branch and the remote-tracking ref follow the server's rewrite.
    let after = head(&sb, &b, "HEAD");
    assert_ne!(before, after, "the landed commit is a new commit");
    assert_eq!(after, head(&sb, &b, "origin/master"));
    let log = text(&ok(&sb, &b, &["log", "--oneline"]).stdout);
    assert_eq!(log.lines().count(), 3, "{log}");
    assert_eq!(
        std::fs::read_to_string(b.join("from-a.txt")).unwrap(),
        "a\n"
    );
    assert_eq!(
        std::fs::read_to_string(b.join("from-b.txt")).unwrap(),
        "b\n"
    );
    assert_eq!(text(&ok(&sb, &b, &["status", "--porcelain"]).stdout), "");

    // `a` sees the same history after a fetch.
    ok(&sb, &a, &["fetch", "origin"]);
    assert_eq!(head(&sb, &a, "origin/master"), after);

    // `-o land` is the same command: `a` moves the branch again, `b` lands on the old tip.
    ok(&sb, &a, &["merge", "--ff-only", "-q", "origin/master"]);
    commit_file(
        &sb,
        &a,
        "later.txt",
        "later
",
    );
    ok(&sb, &a, &["push", "origin", "master"]);
    commit_file(
        &sb,
        &b,
        "again.txt",
        "again
",
    );
    let out = asc(&sb, &b, &["push", "-o", "land", "origin", "master"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("(landed: rebased onto"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(head(&sb, &b, "HEAD"), head(&sb, &b, "origin/master"));
    assert_eq!(
        std::fs::read_to_string(b.join("later.txt")).unwrap(),
        "later
"
    );
}

#[test]
fn land_needs_a_server_remote_and_push_options_need_support() {
    let sb = Sandbox::new();
    let a = sb.dir("a");
    ok(&sb, &a, &["init", "-q"]);
    let bucket = sb.dir("bucket");
    let url = url::Url::from_directory_path(&bucket)
        .unwrap()
        .to_string()
        .trim_end_matches('/')
        .to_owned();
    ok(&sb, &a, &["remote", "add", "origin", &url]);
    commit_file(&sb, &a, "f.txt", "f\n");
    let out = asc(&sb, &a, &["push", "--land", "origin", "master"]);
    assert_eq!(out.status.code(), Some(128));
    assert!(
        text(&out.stderr).contains("--land needs an asc server remote"),
        "{}",
        text(&out.stderr)
    );
    let out = asc(&sb, &a, &["push", "-o", "topic=x", "origin", "master"]);
    assert_eq!(out.status.code(), Some(128));
}

#[test]
fn discovery_objects_sends_get_objects_to_the_tier_and_log_to_the_server() {
    let primary = Server::start(&["acme/app"]);
    let tier = Server::start(&["acme/app"]);
    let sb = Sandbox::new();
    let a = first_client(&sb, "a", &primary.url("acme/app"));
    // The tier holds the objects too (as a cache replica would).
    ok(&sb, &a, &["remote", "add", "tier", &tier.url("acme/app")]);
    let pushed = ok(&sb, &a, &["push", "tier", "master"]);
    assert!(
        tier.count("PutObjects") > 0,
        "{:?} {}",
        tier.calls(),
        text(&pushed.stderr)
    );

    // A document on the remote's own origin may name an objects tier elsewhere only when the
    // user configured it (`remote.<n>.ascObjects`); `ascServer` names the server the same way.
    let wk = WellKnown::start("{}".to_owned());
    primary.forget_calls();
    tier.forget_calls();
    // A second machine: its own `$ASC_HOME`, so every object must come over the wire.
    let sb = Sandbox::new();
    let c = sb.dir("c");
    ok(&sb, &c, &["init", "-q"]);
    ok(&sb, &c, &["remote", "add", "origin", &wk.url("acme/app")]);
    ok(
        &sb,
        &c,
        &["config", "remote.origin.ascServer", &primary.base()],
    );
    ok(
        &sb,
        &c,
        &["config", "remote.origin.ascObjects", &tier.base()],
    );
    let out = asc(&sb, &c, &["fetch", "origin"]);
    assert!(
        out.status.success(),
        "{} tier {:?} primary {:?}",
        text(&out.stderr),
        tier.calls(),
        primary.calls()
    );
    assert!(
        tier.count("GetObjects") > 0,
        "the tier served no GetObjects: tier {:?} primary {:?}",
        tier.calls(),
        primary.calls()
    );
    assert_eq!(
        primary.count("GetObjects"),
        0,
        "objects must come from the tier"
    );
    assert!(primary.count("ListRefs") > 0 || primary.count("Log") > 0);
    assert_eq!(tier.count("Log"), 0, "history questions go to the server");
    assert_eq!(tier.count("ListRefs"), 0, "refs come from the server");

    // `remote.<n>.ascObjects` overrides the document: an empty override list is "no tier".
    ok(
        &sb,
        &c,
        &["config", "remote.origin.ascObjects", &primary.base()],
    );
    tier.forget_calls();
    primary.forget_calls();
    ok(&sb, &c, &["fetch", "origin"]);
    assert_eq!(tier.count("GetObjects"), 0);
}

#[test]
fn a_document_naming_another_origin_gets_no_credential() {
    let primary = Server::start(&["acme/app"]);
    let doc = format!(r#"{{"server":"{}","repos":"*"}}"#, primary.base());
    let wk = WellKnown::start(doc);
    let sb = Sandbox::new();
    let root = sb.tmp.path().to_owned();
    let out = asc(&sb, &root, &["clone", "-q", &wk.url("acme/app"), "c"]);
    assert!(!out.status.success());
    let err = text(&out.stderr);
    let (home, other) = (format!("http://{}", wk.addr), primary.base());
    assert!(err.contains(&home) && err.contains(&other), "{err}");
    assert!(err.contains("different origin"), "{err}");
    assert!(
        primary.calls().is_empty(),
        "the token reached {:?}",
        primary.calls()
    );
}

#[test]
fn plain_http_is_refused_unless_loopback_is_allowed() {
    let server = Server::start(&["acme/app"]);
    let wk = WellKnown::start("{}".to_owned());
    let sb = Sandbox::new();
    first_client(&sb, "a", &server.url("acme/app"));
    server.forget_calls();
    let root = sb.tmp.path().to_owned();
    // The variable is off by default: no discovery request, no RPC, no git.
    let refused = |url: &str| {
        let mut cmd = sb.asc(&root);
        cmd.env_remove("ASC_ALLOW_LOOPBACK_HTTP")
            .env("ASC_TOKEN", WRITE)
            .args(["clone", "-q", url, "x"]);
        let out = cmd.output().expect("spawn asc");
        assert_eq!(out.status.code(), Some(128), "{url}");
        let err = text(&out.stderr);
        assert!(err.contains("refusing plain http"), "{url}: {err}");
        if url.contains("127.0.0.1") {
            assert!(err.contains("ASC_ALLOW_LOOPBACK_HTTP"), "{err}");
        }
    };
    refused(&wk.url("acme/app"));
    refused(&server.url("acme/app"));
    refused(&format!("git+http://{}/acme/app", server.addr));
    refused("http://example.com/acme/app");
    assert!(server.calls().is_empty(), "{:?}", server.calls());
    assert!(!root.join("x").exists());
    // With the opt-in the same loopback remote works.
    ok(&sb, &root, &["clone", "-q", &server.url("acme/app"), "y"]);
}

#[test]
fn a_host_without_a_discovery_document_is_a_git_remote() {
    let sb = Sandbox::new();
    let wk = WellKnown::start("{}".to_owned()); // an invalid document counts as none
    let a = sb.dir("a");
    ok(&sb, &a, &["init", "-q"]);
    ok(&sb, &a, &["remote", "add", "origin", &wk.url("o/r")]);
    let out = asc(&sb, &a, &["auth", "status", "origin"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("mode: git remote"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn credential_failures_follow_git_and_auth_status_names_the_source() {
    let server = Server::start(&["acme/app"]);
    let url = server.url("acme/app");
    let sb = Sandbox::new();
    let a = first_client(&sb, "a", &url);

    // No credential at all and no way to ask for one: the message says what to do.
    let out = asc_as(&sb, &a, None, &["fetch", "origin"]);
    assert_eq!(out.status.code(), Some(128));
    let err = text(&out.stderr);
    assert!(err.contains("authentication required"), "{err}");
    assert!(err.contains("cannot ask for one"), "{err}");

    // A refused credential.
    let out = asc_as(&sb, &a, Some("not-a-token"), &["fetch", "origin"]);
    assert_eq!(out.status.code(), Some(128));
    assert!(
        text(&out.stderr).contains("was refused by"),
        "{}",
        text(&out.stderr)
    );
    assert!(!text(&out.stderr).contains("not-a-token"));

    // A read-only token fetches and cannot push.
    let out = asc_as(&sb, &a, Some(READ), &["fetch", "origin"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    commit_file(&sb, &a, "two.txt", "2\n");
    let out = asc_as(&sb, &a, Some(READ), &["push", "origin", "master"]);
    assert!(!out.status.success());

    // `auth status` names the source, never the secret.
    let out = asc_as(&sb, &a, Some(WRITE), &["auth", "status", "origin"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let s = text(&out.stdout);
    assert!(s.contains("credential source: ASC_TOKEN"), "{s}");
    assert!(s.contains("mode: asc server"), "{s}");
    assert!(!s.contains(WRITE), "{s}");
    let out = asc_as(&sb, &a, None, &["auth", "status", "origin"]);
    assert!(
        text(&out.stdout).contains("credential source: none"),
        "{}",
        text(&out.stdout)
    );

    // http.extraHeader outranks ASC_TOKEN.
    std::fs::write(
        sb.user.join(".gitconfig"),
        format!(
            "[http \"{}/\"]\n\textraHeader = Authorization: Bearer {WRITE}\n",
            server.base()
        ),
    )
    .unwrap();
    let out = asc_as(
        &sb,
        &a,
        Some("ignored"),
        &["auth", "status", "--json", "origin"],
    );
    let s = text(&out.stdout);
    assert!(s.contains("http.extraHeader"), "{s}");
    assert!(!s.contains(WRITE), "{s}");
    let out = asc_as(&sb, &a, None, &["fetch", "origin"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
}

#[test]
fn auth_login_without_a_host_is_a_usage_error() {
    let sb = Sandbox::new();
    let d = sb.dir("d");
    let out = asc_as(&sb, &d, None, &["auth", "login"]);
    assert_eq!(out.status.code(), Some(129));
    assert!(
        text(&out.stderr).contains("asc auth login --host <url>"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn diff_remote_needs_no_workspace() {
    let server = Server::start(&["acme/app"]);
    let url = server.url("acme/app");
    let sb = Sandbox::new();
    let a = first_client(&sb, "a", &url);
    commit_file(&sb, &a, "two.txt", "two\n");
    std::fs::write(a.join("one.txt"), "one\nchanged\n").unwrap();
    ok(&sb, &a, &["add", "one.txt"]);
    ok(&sb, &a, &["commit", "-q", "-m", "edit"]);
    ok(&sb, &a, &["push", "origin", "master"]);
    let (base, tip) = ("master~2".to_owned(), "master".to_owned());

    // Outside any repository, by URL.
    let empty = sb.dir("nowhere");
    let out = asc(
        &sb,
        &empty,
        &["diff", "--remote", &url, "--name-status", &base, &tip],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    let s = text(&out.stdout);
    assert!(s.contains("M\tone.txt"), "{s}");
    assert!(s.contains("A\ttwo.txt"), "{s}");

    // The patch is git's, with content read through the server.
    let out = asc(
        &sb,
        &empty,
        &["diff", "--remote", &url, &base, &tip, "--", "one.txt"],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    let s = text(&out.stdout);
    assert!(s.contains("+changed"), "{s}");
    assert!(!s.contains("two.txt"), "{s}");

    // By configured remote name, inside the workspace, with `..` syntax.
    let out = asc(
        &sb,
        &a,
        &[
            "diff",
            "--remote",
            "origin",
            "--name-only",
            &format!("{base}..{tip}"),
        ],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "one.txt\ntwo.txt\n");
}

/// A parent `mono` whose `sdk` gitlink pins the head of the `sdk` repository, both on `server`
/// and cloned side by side; `sdk` is then a writable mount of `mono`.
fn mono_with_mount(sb: &Sandbox, server: &Server) -> (std::path::PathBuf, std::path::PathBuf) {
    let sdk = first_client(sb, "sdk", &server.url("acme/sdk"));
    let pin = head(sb, &sdk, "HEAD");
    let mono = first_client(sb, "mono", &server.url("acme/mono"));
    let link = format!("160000,{pin},sdk");
    ok(sb, &mono, &["update-index", "--add", "--cacheinfo", &link]);
    ok(sb, &mono, &["commit", "-q", "-m", "link sdk"]);
    ok(sb, &mono, &["push", "-q", "origin", "master"]);
    // `init` names a repository after its directory.
    ok(sb, &mono, &["config", "asc.submodule.sdk.repo", "sdk"]);
    ok(sb, &mono, &["config", "asc.submodule.sdk.branch", "master"]);
    let out = ok(sb, &mono, &["submodule", "edit", "sdk"]);
    assert!(
        text(&out.stdout).contains("writable mount"),
        "{}",
        text(&out.stdout)
    );
    (mono, sdk)
}

#[test]
fn a_push_through_a_writable_mount_moves_both_repositories_together() {
    let server = Server::start(&["acme/mono", "acme/sdk"]);
    let sb = Sandbox::new();
    let (mono, sdk) = mono_with_mount(&sb, &server);
    let sdk_before = head(&sb, &sdk, "origin/master");

    std::fs::write(mono.join("sdk/one.txt"), "changed\n").unwrap();
    std::fs::write(mono.join("one.txt"), "parent\n").unwrap();
    ok(&sb, &mono, &["commit", "-q", "-a", "-m", "both sides"]);
    let out = ok(&sb, &mono, &["push", "origin", "master"]);
    assert!(
        text(&out.stdout).contains("sdk: pushed master (linked: true)"),
        "{}",
        text(&out.stdout)
    );
    assert!(
        server
            .calls()
            .iter()
            .filter(|c| c.contains("UpdateRefs"))
            .count()
            >= 1
    );

    // A clone of the child on another machine sees the commit made in the mount.
    let fresh = Sandbox::new();
    let root = fresh.tmp.path().to_owned();
    ok(
        &fresh,
        &root,
        &["clone", "-q", &server.url("acme/sdk"), "sdk2"],
    );
    let sdk2 = root.join("sdk2");
    assert_ne!(head(&fresh, &sdk2, "HEAD"), sdk_before);
    assert_eq!(
        std::fs::read_to_string(sdk2.join("one.txt")).unwrap(),
        "changed\n"
    );
    let log = text(&ok(&fresh, &sdk2, &["log", "-1", "--format=%s"]).stdout);
    assert_eq!(log, "both sides\n");
}

#[test]
fn a_refused_child_update_leaves_the_parent_unmoved() {
    let server = Server::start(&["acme/mono", "acme/sdk"]);
    let sb = Sandbox::new();
    let (mono, _sdk) = mono_with_mount(&sb, &server);
    let parent_before = text(&ok(&sb, &mono, &["ls-remote", "origin", "master"]).stdout);

    // The child's branch moves under the mount, so the linked update's expectation fails.
    let racer = Sandbox::new();
    let root = racer.tmp.path().to_owned();
    ok(
        &racer,
        &root,
        &["clone", "-q", &server.url("acme/sdk"), "r"],
    );
    let r = root.join("r");
    commit_file(&racer, &r, "race.txt", "race\n");
    ok(&racer, &r, &["push", "-q", "origin", "master"]);

    std::fs::write(mono.join("sdk/one.txt"), "changed\n").unwrap();
    std::fs::write(mono.join("one.txt"), "parent\n").unwrap();
    ok(&sb, &mono, &["commit", "-q", "-a", "-m", "both sides"]);
    let out = asc(&sb, &mono, &["push", "origin", "master"]);
    assert!(!out.status.success(), "{}", text(&out.stdout));
    assert_eq!(
        text(&ok(&sb, &mono, &["ls-remote", "origin", "master"]).stdout),
        parent_before
    );
}
