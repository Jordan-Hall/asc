//! Runs real `turbo` 2.10.11 against the `/v8` cache (PLAN U49). Needs `bunx` and network access:
//! `cargo test -p asc-serve --features it-turbo --test turbo_it -- --ignored`.

#![cfg(feature = "it-turbo")]

use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use asc_git::realgit::RealGit;
use asc_rpc::auth::{Access, StaticTokens};
use asc_serve::config::TurboConfig;
use asc_serve::http::Metrics;
use asc_serve::registry::Extensions;
use asc_serve::turbo::{self, TurboState};
use object_store::local::LocalFileSystem;
use object_store::path::Path as StorePath;

const TURBO: &str = "turbo@2.10.11";
const TOKEN: &str = "it-token";
const SIGNING_KEY: &str = "it-signing-key-of-at-least-32-bytes";

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// A two-package monorepo whose `build` task writes `dist/out.txt`.
fn fixture(root: &Path) {
    write(
        root,
        "package.json",
        r#"{"name":"fixture","private":true,"packageManager":"bun@1.3.0","workspaces":["packages/*"]}"#,
    );
    write(
        root,
        "turbo.json",
        r#"{"$schema":"https://turborepo.com/schema.json","remoteCache":{"enabled":true,"signature":true},"tasks":{"build":{"dependsOn":["^build"],"outputs":["dist/**"]}}}"#,
    );
    for name in ["a", "b"] {
        write(
            root,
            &format!("packages/{name}/package.json"),
            &format!(
                r#"{{"name":"@fx/{name}","version":"1.0.0","scripts":{{"build":"node build.js"}}{}}}"#,
                if name == "b" {
                    r#","dependencies":{"@fx/a":"*"}"#
                } else {
                    ""
                }
            ),
        );
        write(
            root,
            &format!("packages/{name}/build.js"),
            &format!(
                "const fs=require('fs');fs.mkdirSync('dist',{{recursive:true}});fs.writeFileSync('dist/out.txt','{name}');"
            ),
        );
    }
    write(
        root,
        ".gitignore",
        "node_modules\n.turbo\ndist\n.local-cache\n",
    );
    // Turbo hashes against git state; a repository keeps that deterministic.
    for args in [
        &["init", "-q"][..],
        &["add", "-A"],
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "x",
        ],
    ] {
        let ok = RealGit::resolve(None, None)
            .expect("git is installed")
            .command()
            .unwrap()
            .args(args)
            .current_dir(root)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }
}

/// `bun`/`bunx` are `.cmd` shims on Windows, which `Command` does not resolve by itself.
fn bun(prog: &str) -> Command {
    if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", prog]);
        c
    } else {
        Command::new(prog)
    }
}

fn run_turbo(root: &Path, api: &str, key: &str) -> Output {
    let cache_dir = root.join(".local-cache");
    bun("bunx")
        .args([TURBO, "run", "build", "--no-daemon"])
        .arg(format!("--cache-dir={}", cache_dir.display()))
        .arg("--cache=remote:rw")
        .current_dir(root)
        .env("TURBO_API", api)
        .env("TURBO_TOKEN", TOKEN)
        .env("TURBO_TEAM", "acme.web")
        .env("TURBO_REMOTE_CACHE_SIGNATURE_KEY", key)
        .env("TURBO_TELEMETRY_DISABLED", "1")
        .env("DO_NOT_TRACK", "1")
        .output()
        .expect("bunx is required")
}

fn text(o: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

async fn serve(store_dir: &Path) -> String {
    let store = LocalFileSystem::new_with_prefix(store_dir).unwrap();
    let tokens = StaticTokens::new().with_token(TOKEN, "ci", Access::Write);
    let state = TurboState::new(
        Arc::new(tokens),
        Arc::new(store),
        StorePath::default(),
        TurboConfig::default(),
        Arc::new(Metrics::new()),
    )
    .with_trusted_token(TOKEN);
    let ext = Extensions::new();
    turbo::register(&ext, Arc::new(state));
    let app = ext.http.router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "runs bunx turbo@2.10.11 (network)"]
async fn second_run_is_a_remote_hit_and_a_bad_signature_is_rejected() {
    let store_dir = tempfile::tempdir().unwrap();
    let api = serve(store_dir.path()).await;
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    fixture(&repo);
    let install = bun("bun")
        .args(["install"])
        .current_dir(&repo)
        .output()
        .expect("bun is required");
    assert!(install.status.success(), "{}", text(&install));

    let first = run_turbo(&repo, &api, SIGNING_KEY);
    assert!(first.status.success(), "{}", text(&first));
    assert!(
        text(&first).contains("0 cached, 2 total"),
        "{}",
        text(&first)
    );

    // Drop the local cache and outputs: the second run must come from the server.
    let _ = std::fs::remove_dir_all(repo.join(".local-cache"));
    for n in ["a", "b"] {
        let _ = std::fs::remove_dir_all(repo.join(format!("packages/{n}/dist")));
    }
    let second = run_turbo(&repo, &api, SIGNING_KEY);
    assert!(second.status.success(), "{}", text(&second));
    assert!(
        text(&second).contains("2 cached, 2 total"),
        "{}",
        text(&second)
    );
    assert!(repo.join("packages/a/dist/out.txt").exists());

    // A different signing key must make turbo refuse the cached artefacts and rebuild.
    let _ = std::fs::remove_dir_all(repo.join(".local-cache"));
    let third = run_turbo(&repo, &api, "another-signing-key-of-32-bytes-or-more");
    assert!(third.status.success(), "{}", text(&third));
    assert!(
        text(&third).contains("0 cached, 2 total"),
        "{}",
        text(&third)
    );
}
