//! U62 acceptance through the built binary: `asc ci checkout` against a bucket remote.

mod support;

use std::path::Path;
use std::process::Output;

use support::{Sandbox, text};

const IDENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

fn asc(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Output {
    sb.asc(cwd)
        .envs(IDENT)
        .args(args)
        .output()
        .expect("spawn asc")
}

fn ok(sb: &Sandbox, cwd: &Path, args: &[&str]) -> String {
    let out = asc(sb, cwd, args);
    assert!(out.status.success(), "asc {args:?}: {}", text(&out.stderr));
    text(&out.stdout)
}

fn bucket_url(dir: &Path) -> String {
    url::Url::from_directory_path(dir)
        .unwrap()
        .to_string()
        .trim_end_matches('/')
        .to_owned()
}

fn commit_file(sb: &Sandbox, repo: &Path, name: &str, body: &str) {
    std::fs::write(repo.join(name), body).unwrap();
    ok(sb, repo, &["add", name]);
    ok(sb, repo, &["commit", "-q", "-m", name]);
}

struct Job {
    runner_temp: std::path::PathBuf,
    env_file: std::path::PathBuf,
    path_file: std::path::PathBuf,
}

fn checkout(
    sb: &Sandbox,
    url: &str,
    rev: &str,
    base: &str,
    event: &str,
    host: &Path,
) -> (Output, Job) {
    let cwd = sb.dir("job");
    let job = Job {
        runner_temp: cwd.join("tmp"),
        env_file: cwd.join("github_env"),
        path_file: cwd.join("github_path"),
    };
    let out = sb
        .asc(&cwd)
        .envs(IDENT)
        .env("GITHUB_TOKEN", "task-token")
        .env("GITHUB_EVENT_NAME", event)
        .env("RUNNER_TEMP", &job.runner_temp)
        .env("GITHUB_ENV", &job.env_file)
        .env("GITHUB_PATH", &job.path_file)
        .args([
            "ci",
            "checkout",
            "--repo",
            "asc/dds",
            "--rev",
            rev,
            "--base",
            base,
        ])
        .args([
            "--dir",
            "work",
            "--server",
            "https://asc.example",
            "--remote-url",
            url,
        ])
        .arg("--host-cache")
        .arg(host)
        .output()
        .expect("spawn asc");
    (out, job)
}

#[test]
fn checkout_materialises_exports_and_persists_the_tier_stack() {
    let sb = Sandbox::new();
    let url = bucket_url(&sb.dir("bucket"));
    let src = sb.dir("src");
    ok(&sb, &src, &["init", "-q", "--refs", "shared", "--store", &url]);
    commit_file(&sb, &src, "bun.lock", "lock v1\n");
    ok(&sb, &src, &["push", "-q", "-u", "origin", "master"]);
    let base = ok(&sb, &src, &["rev-parse", "HEAD"]).trim().to_owned();
    ok(&sb, &src, &["switch", "-q", "-c", "feature"]);
    commit_file(&sb, &src, "feature.txt", "f\n");
    ok(&sb, &src, &["push", "-q", "origin", "feature"]);
    let head = ok(&sb, &src, &["rev-parse", "HEAD"]).trim().to_owned();

    let host = sb.dir("host-cache");
    let (out, job) = checkout(&sb, &url, &head, "master", "pull_request", &host);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let work = sb.tmp.path().join("job").join("work");
    assert_eq!(
        std::fs::read_to_string(work.join("feature.txt")).unwrap(),
        "f\n"
    );
    assert_eq!(
        std::fs::read_to_string(work.join("bun.lock")).unwrap(),
        "lock v1\n"
    );
    assert_eq!(ok(&sb, &work, &["rev-parse", "HEAD"]).trim(), head);
    assert_eq!(
        ok(&sb, &work, &["rev-parse", "--is-shallow-repository"]).trim(),
        "false"
    );

    let env = std::fs::read_to_string(&job.env_file).unwrap();
    assert!(env.contains(&format!("TURBO_SCM_BASE={base}\n")), "{env}");
    for line in [
        "TURBO_SCM_HEAD=HEAD",
        "TURBO_API=https://asc.example",
        "TURBO_TOKEN=task-token",
        "TURBO_TEAM=asc.dds",
        "TURBO_CACHE=local:rw,remote:r",
        "ASC_TOKEN=task-token",
    ] {
        assert!(env.lines().any(|l| l == line), "missing {line} in {env}");
    }
    let asc_home = job.runner_temp.join("asc-home");
    assert!(
        env.lines()
            .any(|l| l == format!("ASC_HOME={}", asc_home.display()))
    );
    let path = std::fs::read_to_string(&job.path_file).unwrap();
    let shim = job.runner_temp.join("asc-shim");
    assert_eq!(path.trim(), shim.display().to_string());
    assert!(
        shim.join(if cfg!(windows) { "git.exe" } else { "git" })
            .exists()
    );

    // Step 6: the keys are in the repository's common config under ASC_HOME.
    let repos = asc_home.join("repos");
    let common = std::fs::read_dir(&repos)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join(".git")
        .join("config");
    let cfg = std::fs::read_to_string(common).unwrap();
    assert!(cfg.contains("ascServer = https://asc.example"), "{cfg}");
    assert!(cfg.contains("readOnly ="), "{cfg}");
    assert!(cfg.contains("dir ="), "{cfg}");
    // The empty host cache was never written.
    assert_eq!(std::fs::read_dir(&host).unwrap().count(), 0);

    // A later step started with only the exported env reads the base's file through the shim.
    let git = shim.join(if cfg!(windows) { "git.exe" } else { "git" });
    let shown = std::process::Command::new(git)
        .current_dir(&work)
        .env("ASC_HOME", &asc_home)
        .env("ASC_TOKEN", "task-token")
        .args(["show", "master:bun.lock"])
        .output()
        .unwrap();
    assert!(shown.status.success(), "{}", text(&shown.stderr));
    assert_eq!(text(&shown.stdout), "lock v1\n");
}

#[test]
fn push_events_get_the_trusted_cache_mode_and_bad_input_is_refused() {
    let sb = Sandbox::new();
    let url = bucket_url(&sb.dir("bucket"));
    let src = sb.dir("src");
    ok(&sb, &src, &["init", "-q", "--refs", "shared", "--store", &url]);
    commit_file(&sb, &src, "a.txt", "a\n");
    ok(&sb, &src, &["push", "-q", "-u", "origin", "master"]);
    let head = ok(&sb, &src, &["rev-parse", "HEAD"]).trim().to_owned();
    let host = sb.dir("host-cache");
    let (out, job) = checkout(&sb, &url, &head, "master", "push", &host);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let env = std::fs::read_to_string(&job.env_file).unwrap();
    assert!(env.contains("TURBO_CACHE=local:rw,remote:rw\n"), "{env}");

    let (bad, _) = checkout(&sb, &url, "--upload-pack=x", "master", "push", &host);
    assert!(!bad.status.success());
    assert!(text(&bad.stderr).contains("--rev is not a usable revision"));
}
