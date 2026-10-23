//! U32 acceptance: `remote`, `fetch`, `push`, `pull` and `clone` for bucket remotes through the
//! built binary. The two-clone exchange, the rejection text and agent-ref fetching are the
//! compat scenarios `remote-bucket`, `push-reject` and `fetch-agent-refs`; these tests cover the
//! command surface, the failure messages and the state each command leaves behind.

mod support;

use std::path::Path;
use std::process::Output;

use asc_state::state::history_boundary;
use asc_state::{AscHome, StateDb};
use asc_store::refs::RepoKey;
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

fn ok(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Output {
    let out = asc(sb, cwd, args);
    assert!(
        out.status.success(),
        "asc {args:?} failed: {}",
        text(&out.stderr)
    );
    out
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

#[test]
fn remote_management_follows_git() {
    let sb = Sandbox::new();
    let repo = sb.dir("r");
    ok(&sb, &repo, &["init", "-q"]);
    assert_eq!(text(&ok(&sb, &repo, &["remote", "-v"]).stdout), "");
    let url = bucket_url(&sb.dir("bucket"));
    ok(&sb, &repo, &["remote", "add", "origin", &url]);
    let dup = asc(&sb, &repo, &["remote", "add", "origin", &url]);
    assert_eq!(dup.status.code(), Some(3));
    assert_eq!(text(&dup.stderr), "error: remote origin already exists.\n");
    let verbose = text(&ok(&sb, &repo, &["remote", "-v"]).stdout);
    assert_eq!(
        verbose,
        format!(
            "origin\t{url} (fetch) [bucket, refs local to this machine]\norigin\t{url} (push) [bucket, refs local to this machine]\n"
        )
    );
    assert_eq!(text(&ok(&sb, &repo, &["remote"]).stdout), "origin\n");
    assert_eq!(
        text(&ok(&sb, &repo, &["remote", "get-url", "origin"]).stdout),
        format!("{url}\n")
    );
    assert_eq!(
        text(&ok(&sb, &repo, &["config", "remote.origin.ascRefs"]).stdout),
        "local\n"
    );

    ok(&sb, &repo, &["remote", "rename", "origin", "up"]);
    assert_eq!(
        text(&ok(&sb, &repo, &["config", "remote.up.url"]).stdout),
        format!("{url}\n")
    );
    assert_eq!(
        text(&ok(&sb, &repo, &["config", "remote.up.fetch"]).stdout),
        "+refs/heads/*:refs/remotes/up/*\n"
    );
    let gone = asc(&sb, &repo, &["remote", "rename", "origin", "x"]);
    assert_eq!(gone.status.code(), Some(2));
    assert_eq!(text(&gone.stderr), "error: No such remote: 'origin'\n");

    let other = bucket_url(&sb.dir("bucket2"));
    ok(&sb, &repo, &["remote", "set-url", "up", &other]);
    assert_eq!(
        text(&ok(&sb, &repo, &["remote", "get-url", "up"]).stdout),
        format!("{other}\n")
    );
    ok(&sb, &repo, &["remote", "remove", "up"]);
    let again = asc(&sb, &repo, &["remote", "remove", "up"]);
    assert_eq!(again.status.code(), Some(2));
    assert_eq!(text(&again.stderr), "error: No such remote: 'up'\n");
    assert_eq!(text(&ok(&sb, &repo, &["remote"]).stdout), "");
}

#[test]
fn hostile_remote_arguments_are_refused() {
    let sb = Sandbox::new();
    let repo = sb.dir("r");
    ok(&sb, &repo, &["init", "-q"]);
    for bad in ["--upload-pack=evil", "-oProxyCommand=x"] {
        let out = asc(&sb, &repo, &["remote", "add", "x", bad]);
        assert_ne!(out.status.code(), Some(0), "{bad}");
    }
    for name in ["a b", "x..y", ".hidden", "a/"] {
        let out = asc(&sb, &repo, &["remote", "add", name, "file:///tmp/x"]);
        assert_eq!(out.status.code(), Some(128), "{name:?}");
    }
    let share = asc(&sb, &repo, &["remote", "add", "x", "file://host/share/b"]);
    assert_eq!(share.status.code(), Some(128));
    assert!(
        text(&share.stderr).contains("network share"),
        "{}",
        text(&share.stderr)
    );
    assert_eq!(text(&ok(&sb, &repo, &["remote"]).stdout), "");
}

#[test]
fn a_missing_remote_fails_the_way_git_does() {
    let sb = Sandbox::new();
    let repo = sb.dir("r");
    ok(&sb, &repo, &["init", "-q"]);
    for cmd in ["fetch", "push"] {
        let out = asc(&sb, &repo, &[cmd]);
        assert_eq!(out.status.code(), Some(128), "{cmd}");
        assert!(
            text(&out.stderr)
                .starts_with("fatal: 'origin' does not appear to be a git repository\n"),
            "{cmd}: {}",
            text(&out.stderr)
        );
    }
}

#[test]
fn a_push_without_upstream_says_how_to_set_one() {
    let sb = Sandbox::new();
    let repo = sb.dir("r");
    ok(
        &sb,
        &repo,
        &["init", "-q", "--store", &bucket_url(&sb.dir("bucket"))],
    );
    commit_file(&sb, &repo, "a.txt", "a\n");
    let out = asc(&sb, &repo, &["push"]);
    assert_eq!(out.status.code(), Some(128));
    let err = text(&out.stderr);
    assert!(
        err.starts_with("fatal: The current branch master has no upstream branch.\n"),
        "{err}"
    );
    assert!(
        err.contains("asc push --set-upstream origin master"),
        "{err}"
    );
}

#[test]
fn push_porcelain_dry_run_and_delete() {
    let sb = Sandbox::new();
    let bucket = sb.dir("bucket");
    let url = bucket_url(&bucket);
    let repo = sb.dir("r");
    ok(&sb, &repo, &["init", "-q", "--store", &url]);
    commit_file(&sb, &repo, "a.txt", "a\n");

    // A dry run uploads nothing and moves nothing.
    let dry = asc(&sb, &repo, &["push", "--dry-run", "origin", "master"]);
    assert!(dry.status.success());
    assert!(
        text(&dry.stderr).contains(" * [new branch]      master -> master\n"),
        "{}",
        text(&dry.stderr)
    );
    assert_eq!(
        std::fs::read_dir(&bucket).unwrap().count(),
        0,
        "a dry run wrote to the bucket"
    );

    let pushed = ok(&sb, &repo, &["push", "--porcelain", "origin", "master"]);
    assert_eq!(
        text(&pushed.stdout),
        format!("To {url}\n*\trefs/heads/master:refs/heads/master\t[new branch]\nDone\n")
    );
    assert!(
        bucket.join("refs-local").is_file(),
        "the first push writes refs-local"
    );
    assert_eq!(
        text(&ok(&sb, &repo, &["rev-parse", "origin/master"]).stdout),
        text(&ok(&sb, &repo, &["rev-parse", "master"]).stdout)
    );

    let del = asc(&sb, &repo, &["push", "origin", "--delete", "master"]);
    assert!(del.status.success(), "{}", text(&del.stderr));
    assert!(
        text(&del.stderr).contains(" - [deleted]         master\n"),
        "{}",
        text(&del.stderr)
    );
    let missing = asc(&sb, &repo, &["push", "origin", "--delete", "master"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(text(&missing.stderr).contains("unable to delete 'master': remote ref does not exist"));

    let opt = asc(&sb, &repo, &["push", "-o", "ci.skip", "origin", "master"]);
    assert_eq!(opt.status.code(), Some(128));
    assert!(text(&opt.stderr).contains("the receiving end does not support push options"));
    let unknown = asc(&sb, &repo, &["push", "origin", "nope"]);
    assert_eq!(unknown.status.code(), Some(1));
    assert!(text(&unknown.stderr).contains("error: src refspec nope does not match any\n"));
}

#[test]
fn clone_failures_leave_nothing_behind() {
    let sb = Sandbox::new();
    let cwd = sb.dir("cwd");
    let missing = bucket_url(&cwd.join("no-such-bucket"));
    let out = asc(&sb, &cwd, &["clone", &missing, "dst"]);
    assert_eq!(out.status.code(), Some(128));
    assert!(
        text(&out.stderr).contains(&format!("fatal: repository '{missing}' does not exist")),
        "{}",
        text(&out.stderr)
    );
    assert!(!cwd.join("dst").exists());

    let other = asc(
        &sb,
        &cwd,
        &["clone", "https://example.invalid/o/r.git", "dst2"],
    );
    assert_eq!(other.status.code(), Some(128));
    assert!(
        text(&other.stderr).contains("not supported yet"),
        "{}",
        text(&other.stderr)
    );
    assert!(!cwd.join("dst2").exists());

    let busy = cwd.join("busy");
    std::fs::create_dir(&busy).unwrap();
    std::fs::write(busy.join("x"), "x").unwrap();
    let bucket = sb.dir("bucket");
    let out = asc(&sb, &cwd, &["clone", &bucket_url(&bucket), "busy"]);
    assert_eq!(out.status.code(), Some(128));
    assert!(
        text(&out.stderr)
            .contains("destination path 'busy' already exists and is not an empty directory.")
    );
}

#[test]
fn init_store_wants_a_bucket() {
    let sb = Sandbox::new();
    let cwd = sb.dir("cwd");
    let out = asc(
        &sb,
        &cwd,
        &["init", "-q", "--store", "git@example.invalid:o/r.git", "x"],
    );
    assert_eq!(out.status.code(), Some(128));
    assert!(
        text(&out.stderr).contains("--store needs a bucket URL"),
        "{}",
        text(&out.stderr)
    );
    assert!(!cwd.join("x").join(".git").exists());
    let refs = asc(&sb, &cwd, &["init", "-q", "--refs", "local", "y"]);
    assert_eq!(refs.status.code(), Some(128));
}

#[test]
fn cloning_an_empty_bucket_warns_and_configures_the_remote() {
    let sb = Sandbox::new();
    let bucket = sb.dir("bucket");
    let url = bucket_url(&bucket);
    let cwd = sb.dir("cwd");
    let out = asc(&sb, &cwd, &["clone", "-q", &url, "c"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("warning: You appear to have cloned an empty repository."));
    let c = cwd.join("c");
    assert_eq!(
        text(&ok(&sb, &c, &["config", "remote.origin.url"]).stdout),
        format!("{url}\n")
    );
    let fetch = text(&ok(&sb, &c, &["config", "--get-all", "remote.origin.fetch"]).stdout);
    assert_eq!(
        fetch,
        "+refs/heads/*:refs/remotes/origin/*\n^refs/heads/agent/*\n"
    );
}

#[test]
fn a_pull_of_divergent_branches_asks_how_to_reconcile() {
    let sb = Sandbox::new();
    let url = bucket_url(&sb.dir("bucket"));
    let a = sb.dir("a");
    ok(&sb, &a, &["init", "-q", "--store", &url]);
    commit_file(&sb, &a, "f.txt", "base\n");
    ok(&sb, &a, &["push", "-q", "-u", "origin", "master"]);
    let cwd = sb.dir("cwd");
    ok(&sb, &cwd, &["clone", "-q", &url, "b"]);
    let b = cwd.join("b");
    commit_file(&sb, &b, "b.txt", "b\n");
    ok(&sb, &b, &["push", "-q"]);
    commit_file(&sb, &a, "a.txt", "a\n");
    let out = asc(&sb, &a, &["pull"]);
    assert_eq!(out.status.code(), Some(128));
    let err = text(&out.stderr);
    assert!(
        err.contains("fatal: Need to specify how to reconcile divergent branches."),
        "{err}"
    );
    // A stated policy goes through: the rebase replays a's commit over b's.
    ok(&sb, &a, &["pull", "-q", "--rebase"]);
    assert_eq!(std::fs::read_to_string(a.join("b.txt")).unwrap(), "b\n");
    ok(&sb, &a, &["push", "-q"]);
}

#[test]
fn boundary_store_rows_round_trip_per_repository() {
    let sb = Sandbox::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let home = AscHome::open(sb.asc_home.clone()).unwrap();
        let db = StateDb::open(&home).await.unwrap();
        let (r1, r2) = (RepoKey(1), RepoKey(2));
        let ids: Vec<_> = (1u8..=3)
            .map(|n| asc_core::id::ChangesetId::from_bytes([n; 32]))
            .collect();
        assert!(history_boundary::list(&db, r1).await.unwrap().is_empty());
        for id in &ids[..2] {
            history_boundary::insert(&db, r1, *id).await.unwrap();
        }
        history_boundary::insert(&db, r2, ids[2]).await.unwrap();
        assert_eq!(
            history_boundary::list(&db, r1).await.unwrap(),
            ids[..2].to_vec()
        );
        assert_eq!(history_boundary::list(&db, r2).await.unwrap(), vec![ids[2]]);
        history_boundary::remove(&db, r1, ids[0]).await.unwrap();
        assert_eq!(history_boundary::list(&db, r1).await.unwrap(), vec![ids[1]]);
    });
}

/// A token in a remote URL's userinfo must reach neither the terminal nor the ref logs.
#[test]
fn a_token_in_a_remote_url_is_never_printed_or_recorded() {
    const TOKEN: &str = "SECRETTOKEN123";
    let sb = Sandbox::new();
    let repo = sb.dir("r");
    ok(&sb, &repo, &["init", "-q"]);
    commit_file(&sb, &repo, "a.txt", "a\n");
    let urls = [
        format!("s3://user:{TOKEN}@nobucket/p?endpoint=http://127.0.0.1:9"),
        format!("https://user:{TOKEN}@127.0.0.1:9/o/r.git"),
        format!("user:{TOKEN}@127.0.0.1:o/r.git"),
    ];
    for (i, url) in urls.iter().enumerate() {
        let name = format!("r{i}");
        ok(&sb, &repo, &["remote", "add", &name, url]);
        for cmd in [
            ["fetch", name.as_str()],
            ["push", name.as_str()],
            ["pull", name.as_str()],
        ] {
            let out = asc(&sb, &repo, &cmd);
            let (so, se) = (text(&out.stdout), text(&out.stderr));
            assert!(
                !so.contains(TOKEN) && !se.contains(TOKEN),
                "{cmd:?} on {url}: {so}{se}"
            );
        }
        let direct = asc(&sb, &repo, &["fetch", url]);
        assert!(
            !text(&direct.stderr).contains(TOKEN),
            "fetch {url}: {}",
            text(&direct.stderr)
        );
        let dest = sb.dir(&format!("c{i}"));
        let clone = asc(&sb, &sb.dir("w"), &["clone", url, dest.to_str().unwrap()]);
        let (so, se) = (text(&clone.stdout), text(&clone.stderr));
        assert!(
            !so.contains(TOKEN) && !se.contains(TOKEN),
            "clone {url}: {so}{se}"
        );
    }
    // Neither the reflogs nor FETCH_HEAD carry the token.
    let mut stack = vec![repo.clone(), sb.dir("w")];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().is_some_and(|n| n == "FETCH_HEAD")
                || p.to_string_lossy().contains("logs")
            {
                let body = std::fs::read(&p).unwrap_or_default();
                assert!(!text(&body).contains(TOKEN), "{}", p.display());
            }
        }
    }
}

#[test]
fn remote_verbose_prints_the_configured_url_as_git_does() {
    let sb = Sandbox::new();
    let repo = sb.dir("r");
    ok(&sb, &repo, &["init", "-q"]);
    ok(
        &sb,
        &repo,
        &["remote", "add", "o", "user:pw@127.0.0.1:o/r.git"],
    );
    let v = text(&ok(&sb, &repo, &["remote", "-v"]).stdout);
    assert!(v.starts_with("o\tuser:pw@127.0.0.1:o/r.git (fetch)"), "{v}");
}
