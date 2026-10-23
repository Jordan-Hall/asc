//! U51 acceptance: the deploy bundle end to end. Needs the stack from `deploy/compose.yml`:
//!
//! ```text
//! docker compose -f deploy/compose.yml up -d --wait
//! cargo test -p asc --test deploy_smoke -- --ignored
//! ```
//!
//! The test starts nothing. It mints an admin token inside the running `asc-primary` container,
//! initialises a fresh primary repository through `RepoAdmin`, then clones, pushes, lands and
//! runs `diff --remote` with the built `asc` binary, puts and gets a turbo artefact, and scrapes
//! `/metrics` from the admin listener. Every endpoint is a loopback port the compose file
//! publishes; plain http works only because the client sets the loopback opt-in.

mod support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use support::{Sandbox, text};

const PRIMARY: &str = "http://127.0.0.1:17890";
const CACHE: &str = "http://127.0.0.1:17891";
const ADMIN: &str = "http://127.0.0.1:19464";
/// The public development token of `deploy/compose.yml`.
const ADMIN_TOKEN: &str = "asc-deploy-admin-token-not-secret";

const IDENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "Smoke"),
    ("GIT_AUTHOR_EMAIL", "smoke@example.com"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_NAME", "Smoke"),
    ("GIT_COMMITTER_EMAIL", "smoke@example.com"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

fn compose_file() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/compose.yml")
}

/// A token with admin rights on `smoke/*`, minted inside the running primary container.
fn mint_token() -> String {
    let out = Command::new("docker")
        .args(["compose", "-f"])
        .arg(compose_file())
        .args([
            "exec",
            "-T",
            "asc-primary",
            "asc",
            "serve",
            "token",
            "create",
            "--user",
            "smoke",
            "--repo",
            "smoke/*",
            "--admin",
            "--expires",
            "1d",
            "--label",
            "deploy-smoke",
        ])
        .output()
        .expect("run docker; is the stack up? docker compose -f deploy/compose.yml up -d --wait");
    assert!(
        out.status.success(),
        "token create failed: {}",
        text(&out.stderr)
    );
    let token = text(&out.stdout).trim().to_owned();
    assert!(!token.is_empty() && !token.contains(char::is_whitespace));
    token
}

fn asc(sb: &Sandbox, cwd: &Path, token: &str, args: &[&str]) -> Output {
    sb.asc(cwd)
        .envs(IDENT)
        .env("ASC_TOKEN", token)
        .args(args)
        .output()
        .expect("spawn asc")
}

fn ok(sb: &Sandbox, cwd: &Path, token: &str, args: &[&str]) -> Output {
    let out = asc(sb, cwd, token, args);
    assert!(
        out.status.success(),
        "asc {args:?} failed: {}",
        text(&out.stderr)
    );
    out
}

fn commit_file(sb: &Sandbox, repo: &Path, token: &str, name: &str, body: &str) {
    std::fs::write(repo.join(name), body).unwrap();
    ok(sb, repo, token, &["add", name]);
    ok(sb, repo, token, &["commit", "-q", "-m", name]);
}

fn rev(sb: &Sandbox, repo: &Path, token: &str, r: &str) -> String {
    text(&ok(sb, repo, token, &["rev-parse", r]).stdout)
        .trim()
        .to_owned()
}

/// `curl` with `args`; returns the status code and the body. `body` goes to stdin.
fn curl(args: &[&str], body: Option<&[u8]>) -> (u16, Vec<u8>) {
    let mut child = Command::new("curl")
        .args(["-sS", "--max-time", "30", "-w", "\n%{http_code}"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn curl");
    let mut stdin = child.stdin.take().unwrap();
    if let Some(b) = body {
        stdin.write_all(b).unwrap();
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "curl {args:?} failed");
    let split = out.stdout.iter().rposition(|b| *b == b'\n').unwrap();
    let code = std::str::from_utf8(&out.stdout[split + 1..])
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    (code, out.stdout[..split].to_vec())
}

#[test]
#[ignore = "needs the stack from deploy/compose.yml"]
fn the_deploy_bundle_serves_a_repository_end_to_end() {
    // Both servers are up and the cache role is ready (compose `--wait` already checked this).
    assert_eq!(curl(&[&format!("{PRIMARY}/healthz")], None).0, 200);
    assert_eq!(curl(&[&format!("{CACHE}/readyz")], None).0, 200);

    let token = mint_token();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("smoke/r{nanos}");
    let url = format!("{PRIMARY}/{name}");
    let sb = Sandbox::new();
    let root = sb.tmp.path().to_owned();

    // Init a primary repository through admin; the default branch is master.
    let req = format!(r#"{{"repo":{{"name":"{name}"}},"init":{{"defaultBranch":"master"}}}}"#);
    ok(
        &sb,
        &root,
        &token,
        &["api", "RepoAdmin", "--remote", &url, "--json", &req],
    );

    // Push: the first commit from client a.
    let a = sb.dir("a");
    ok(&sb, &a, &token, &["init", "-q"]);
    ok(&sb, &a, &token, &["remote", "add", "origin", &url]);
    commit_file(&sb, &a, &token, "one.txt", "one\n");
    let pushed = text(&ok(&sb, &a, &token, &["push", "origin", "master"]).stderr);
    assert!(pushed.contains("master -> master"), "{pushed}");

    // Clone.
    ok(&sb, &root, &token, &["clone", "-q", &url, "b"]);
    let b = root.join("b");
    assert_eq!(std::fs::read_to_string(b.join("one.txt")).unwrap(), "one\n");

    // Land: a moves master; b, still on the old tip, lands on top of it.
    commit_file(&sb, &a, &token, "from-a.txt", "a\n");
    ok(&sb, &a, &token, &["push", "origin", "master"]);
    commit_file(&sb, &b, &token, "from-b.txt", "b\n");
    let before = rev(&sb, &b, &token, "HEAD");
    let out = asc(&sb, &b, &token, &["push", "--land", "origin", "master"]);
    let err = text(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("(landed: rebased onto"), "{err}");
    let after = rev(&sb, &b, &token, "HEAD");
    assert_ne!(before, after, "landing rewrites the commit");
    assert_eq!(after, rev(&sb, &b, &token, "origin/master"));

    // diff --remote needs no workspace.
    let nowhere = sb.dir("nowhere");
    let out = ok(
        &sb,
        &nowhere,
        &token,
        &[
            "diff",
            "--remote",
            &url,
            "--name-status",
            "master~2",
            "master",
        ],
    );
    let s = text(&out.stdout);
    assert!(
        s.contains("A\tfrom-a.txt") && s.contains("A\tfrom-b.txt"),
        "{s}"
    );

    // Turbo cache put and get on the primary (the slug is owner.repo).
    let slug = name.replace('/', ".");
    let art = format!("{PRIMARY}/v8/artifacts/0123456789abcdef?slug={slug}");
    let bearer = format!("Authorization: Bearer {token}");
    let payload = b"turbo artefact bytes";
    let (code, _) = curl(
        &[
            "-X",
            "PUT",
            "-H",
            &bearer,
            "-H",
            "x-artifact-tag: sig-smoke",
            "-H",
            "x-artifact-duration: 12",
            "--data-binary",
            "@-",
            &art,
        ],
        Some(payload),
    );
    assert!((200..300).contains(&code), "turbo put: {code}");
    let (code, body) = curl(&["-H", &bearer, &art], None);
    assert_eq!(code, 200, "turbo get");
    assert_eq!(body, payload);

    // Metrics scrape on the admin listener: refused without the token, served with it.
    assert_eq!(curl(&[&format!("{ADMIN}/metrics")], None).0, 401);
    let admin_bearer = format!("Authorization: Bearer {ADMIN_TOKEN}");
    let (code, body) = curl(&["-H", &admin_bearer, &format!("{ADMIN}/metrics")], None);
    assert_eq!(code, 200);
    let metrics = String::from_utf8_lossy(&body);
    assert!(metrics.contains("asc_rpc_duration_seconds"), "{metrics}");
    // The public listener never serves /metrics.
    assert_eq!(curl(&[&format!("{PRIMARY}/metrics")], None).0, 404);
}
