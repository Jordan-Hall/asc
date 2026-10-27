//! Client cases against the real `asc serve`, each compared with a plain git server where git's
//! own output is the reference: clone, fetch, pull, push, shallow, partial, protocol versions,
//! a submodule, the credential paths of U79 repeated on the live layer, and the whole-stack
//! security and limit cases (SPEC §25.5, §25.8, §25.9).

use std::path::{Path, PathBuf};

use regex::Regex;

use crate::harness::*;
use crate::plain::Plain;

/// Marks a script written by a test as runnable, which git needs to exec it as a helper on Unix.
fn make_executable(script: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[cfg(not(unix))]
    let _ = script;
}

/// One asc server and one plain git server, each holding an empty `acme/web` (and `acme/lib`).
pub struct Rig {
    pub stack: Stack,
    pub plain: Plain,
    pub git: Git,
    pub write: Token,
    pub read: Token,
    _plain_dir: tempfile::TempDir,
}

impl Rig {
    pub fn new() -> Self {
        // The 10,000-commit fixture adds more files than the default secret scan reads; a blocking
        // scan that stopped early refuses the push, which is not what these tests compare.
        let stack = Stack::start_with(
            "enabled = true\n",
            "",
            "[secrets]\nmax_files = 200000\nmax_scan_bytes = \"1GiB\"\n",
        );
        for repo in ["acme/web", "acme/lib"] {
            stack.create_repo(repo, "");
        }
        let write = stack.token(&["--user", "alice", "--repo", "acme/*", "--write"]);
        let read = stack.token(&["--user", "bob", "--repo", "acme/*", "--read"]);
        let git = Git::new();
        let plain_dir = tempfile::tempdir().unwrap();
        let plain = Plain::start(&git, plain_dir.path());
        for repo in ["acme/web", "acme/lib"] {
            plain.create(&git, repo);
        }
        Self {
            stack,
            plain,
            git,
            write,
            read,
            _plain_dir: plain_dir,
        }
    }

    pub fn asc(&self, repo: &str) -> String {
        self.stack.url_with(repo, &self.write)
    }

    pub fn plain(&self, repo: &str) -> String {
        self.plain.url(repo)
    }

    /// The URLs and secrets a message may hold, for [`norm`].
    pub fn scrub(&self) -> Vec<String> {
        let mut v = vec![self.write.secret.clone(), self.read.secret.clone()];
        for r in ["acme/web", "acme/lib"] {
            v.push(self.asc(r));
            v.push(self.stack.url(r));
            v.push(self.plain(r));
        }
        v.push(format!("http://{}", self.stack.host()));
        v.push(format!("http://{}", self.plain.addr));
        v.sort_by_key(|s| std::cmp::Reverse(s.len()));
        v
    }
}

/// git's client text with everything that legitimately differs between hosts removed: URLs,
/// secrets, object ids, `remote:` lines (a host's own words), git's `warning: redirecting to`
/// line (a client that answers a challenge reports the base URL it ends on, though nothing was
/// redirected: it depends on the git and libcurl in use) and blank lines.
pub fn norm(text: &str, scrub: &[String]) -> Vec<String> {
    let hex = Regex::new(r"\b[0-9a-f]{7,40}\b").unwrap();
    text.lines()
        .filter(|l| !l.starts_with("remote:") && !l.starts_with("warning: redirecting to "))
        .map(|l| {
            let mut l = l.to_owned();
            for s in scrub {
                l = l.replace(s.as_str(), "<URL>");
            }
            hex.replace_all(&l, "<sha>").into_owned()
        })
        .filter(|l| !l.trim().is_empty())
        .collect()
}

/// `n` commits of an ASC-shaped tree (directories of files, a changing hot file), one commit a
/// day from 2023-11-14, written with `git fast-import`: a repository `name` with branch master.
pub fn fixture(git: &Git, name: &str, n: usize) -> PathBuf {
    let w = git.work(name);
    git.ok(&w, &["init", "-q"]);
    let mut stream = String::new();
    for i in 0..n {
        let msg = format!("c{i}");
        stream.push_str(&format!(
            "commit refs/heads/master\nmark :{m}\ncommitter T <t@x> {t} +0000\ndata {l}\n{msg}\n",
            m = i + 1,
            t = 1_700_000_000 + (i as u64) * 86_400,
            l = msg.len()
        ));
        if i > 0 {
            stream.push_str(&format!("from :{i}\n"));
        }
        let file = format!("pkg{}/file{}.txt", i % 40, i % 250);
        let body = format!("revision {i}\n");
        stream.push_str(&format!(
            "M 100644 inline {file}\ndata {}\n{body}\n",
            body.len()
        ));
        let hot = format!("hot {i}\n");
        stream.push_str(&format!(
            "M 100644 inline hot.txt\ndata {}\n{hot}\n",
            hot.len()
        ));
        if i % 100 == 0 {
            let big = format!("{}\n", "x".repeat(2000 + i));
            stream.push_str(&format!(
                "M 100644 inline big/blob{i}.txt\ndata {}\n{big}\n",
                big.len()
            ));
        }
    }
    git.ok_stdin(&w, &["fast-import", "--quiet"], stream.as_bytes());
    git.ok(&w, &["checkout", "-q", "-f", "master"]);
    w
}

fn ls_remote(git: &Git, url: &str, version: &str) -> String {
    let cwd = git.work("lsr");
    let r = git.run(
        &cwd,
        &[
            "-c",
            &format!("protocol.version={version}"),
            "ls-remote",
            url,
        ],
    );
    assert!(r.ok, "ls-remote v{version}: {}", mask(&r.stderr));
    r.stdout
}

/// Pushes `src` (all branches and tags) to `url`.
fn seed(git: &Git, src: &Path, url: &str) {
    let r = git.run(src, &["push", "--mirror", url]);
    assert!(r.ok, "seed push: {}", mask(&r.stderr));
}

// =================================================================================== clone

#[test]
fn clone_of_an_empty_a_small_and_a_asc_shaped_repo_matches_a_plain_server() {
    let rig = Rig::new();
    let (git, scrub) = (&rig.git, rig.scrub());

    // Empty: git's own warning, the same on both.
    let (c, p) = (git.work("empty-asc"), git.work("empty-plain"));
    let rc = git.run(&c, &["clone", &rig.asc("acme/web"), "w"]);
    let rp = git.run(&p, &["clone", &rig.plain("acme/web"), "w"]);
    assert!(rc.ok && rp.ok, "{}\n{}", mask(&rc.stderr), rp.stderr);
    assert_eq!(norm(&rc.stderr, &scrub), norm(&rp.stderr, &scrub));

    // Small and ASC-shaped (10,000 commits) from the same source.
    let small = fixture(git, "small", 5);
    let asc = fixture(git, "asc", 10_000);
    for (name, src, repo) in [("small", &small, "acme/lib"), ("asc", &asc, "acme/web")] {
        seed(git, src, &rig.asc(repo));
        seed(git, src, &rig.plain(repo));
        let (c, p) = (
            git.work(&format!("{name}-asc")),
            git.work(&format!("{name}-plain")),
        );
        let rc = git.run(&c, &["clone", "-q", &rig.asc(repo), "w"]);
        let rp = git.run(&p, &["clone", "-q", &rig.plain(repo), "w"]);
        assert!(rc.ok, "{name}: {}", mask(&rc.stderr));
        assert!(rp.ok, "{name}: {}", rp.stderr);
        let (cw, pw) = (c.join("w"), p.join("w"));
        assert_eq!(git.head(&cw), git.head(&src.to_owned()), "{name}: head");
        assert_eq!(git.head(&cw), git.head(&pw));
        assert_eq!(
            git.ok(&cw, &["rev-list", "--count", "HEAD"]),
            git.ok(&pw, &["rev-list", "--count", "HEAD"])
        );
        assert_eq!(refs_of(git, &cw), refs_of(git, &pw));
        let fsck = git.run(&cw, &["fsck", "--strict"]);
        assert!(fsck.ok, "{name}: fsck: {}", fsck.stderr);
        assert_eq!(
            git.ok(&cw, &["ls-tree", "-r", "HEAD"]),
            git.ok(&pw, &["ls-tree", "-r", "HEAD"])
        );
    }
    assert_eq!(
        git.ok(
            &git.work("asc-asc").join("w"),
            &["rev-list", "--count", "HEAD"]
        ),
        "10000"
    );
}

#[test]
fn fetch_and_pull_ff_only_match_a_plain_server() {
    let rig = Rig::new();
    let (git, scrub) = (&rig.git, rig.scrub());
    let src = fixture(git, "src", 3);
    seed(git, &src, &rig.asc("acme/web"));
    seed(git, &src, &rig.plain("acme/web"));
    let mut out = Vec::new();
    for (who, url) in [
        ("asc", rig.asc("acme/web")),
        ("plain", rig.plain("acme/web")),
    ] {
        let base = git.work(who);
        git.ok(&base, &["clone", "-q", &url, "w"]);
        let w = base.join("w");
        // The origin moves (a second clone pushes), then fetch and pull --ff-only.
        let other = git.work(&format!("{who}-other"));
        git.ok(&other, &["clone", "-q", &url, "o"]);
        let o = other.join("o");
        std::fs::write(o.join("new.txt"), "new\n").unwrap();
        git.ok(&o, &["add", "."]);
        git.ok(&o, &["commit", "-q", "-m", "new"]);
        git.ok(&o, &["tag", "-a", "v9", "-m", "nine"]);
        let r = git.run(&o, &["push", "origin", "master", "v9"]);
        assert!(r.ok, "{who}: push {}", mask(&r.stderr));
        let f = git.run(&w, &["fetch", "origin"]);
        assert!(f.ok, "{who}: fetch {}", mask(&f.stderr));
        let p = git.run(&w, &["pull", "--ff-only", "origin", "master"]);
        assert!(p.ok, "{who}: pull {}", mask(&p.stderr));
        assert_eq!(git.head(&w), git.head(&o), "{who}: pulled the new commit");
        let f2 = git.run(&w, &["fetch", "origin"]);
        assert!(f2.ok);
        out.push((
            norm(&f.stderr, &scrub),
            norm(&p.stdout, &scrub),
            git.ok(&w, &["tag", "-l"]),
            git.ok(&w, &["log", "--format=%H %s"]),
        ));
    }
    assert_eq!(out[0], out[1], "git's output is the same on both servers");
}

// =================================================================================== push

type Step = (String, bool, Vec<String>);

fn push_steps(rig: &Rig, who: &str, url: &str) -> Vec<Step> {
    let (git, scrub) = (&rig.git, rig.scrub());
    let mut log: Vec<Step> = Vec::new();
    let mut note = |label: &str, r: Run| {
        log.push((label.to_owned(), r.ok, norm(&r.stderr, &scrub)));
    };
    let base = git.work(who);
    let commit = |dir: &Path, file: &str, body: &str, msg: &str| {
        std::fs::write(dir.join(file), body).unwrap();
        git.ok(dir, &["add", "."]);
        git.ok(dir, &["commit", "-q", "-m", msg]);
    };

    note("clone empty", git.run(&base, &["clone", url, "a"]));
    let a = base.join("a");
    commit(&a, "README", "one\n", "one");
    note(
        "push new branch",
        git.run(&a, &["push", "origin", "master"]),
    );
    commit(&a, "README", "two\n", "two");
    note(
        "push fast-forward",
        git.run(&a, &["push", "origin", "master"]),
    );

    // A second clone pushes first; the first push is then behind and git refuses it itself.
    git.ok(&base, &["clone", "-q", url, "b"]);
    let b = base.join("b");
    commit(&b, "b.txt", "b\n", "from b");
    note("push from b", git.run(&b, &["push", "origin", "master"]));
    commit(&a, "a.txt", "a\n", "from a");
    note(
        "push non-fast-forward",
        git.run(&a, &["push", "origin", "master"]),
    );

    // Branches, deletion, tags, atomic.
    git.ok(&a, &["checkout", "-q", "-B", "feat"]);
    note("push feature", git.run(&a, &["push", "origin", "feat"]));
    note("delete feature", git.run(&a, &["push", "origin", ":feat"]));
    git.ok(&a, &["tag", "v1"]);
    git.ok(&a, &["tag", "-a", "v2", "-m", "release two"]);
    note("push tags", git.run(&a, &["push", "origin", "v1", "v2"]));
    note(
        "atomic ok",
        git.run(
            &a,
            &["push", "--atomic", "origin", "feat:side1", "feat:side2"],
        ),
    );
    note(
        "atomic reject",
        git.run(
            &a,
            &["push", "--atomic", "origin", "feat:side3", "master:master"],
        ),
    );
    log
}

#[test]
fn push_matrix_matches_a_plain_server() {
    let rig = Rig::new();
    let asc = push_steps(&rig, "asc", &rig.asc("acme/web"));
    let plain = push_steps(&rig, "plain", &rig.plain("acme/web"));
    assert_eq!(asc.len(), plain.len());
    for (c, p) in asc.iter().zip(&plain) {
        assert_eq!(c.0, p.0);
        assert_eq!(c.1, p.1, "{}: the same outcome (asc: {:?})", c.0, c.2);
        assert_eq!(c.2, p.2, "{}: the same text from git", c.0);
    }
    // The non-fast-forward is git's own refusal, with its words.
    let nff = asc.iter().find(|s| s.0 == "push non-fast-forward").unwrap();
    assert!(!nff.1, "the push behind is refused");
    assert!(
        nff.2
            .iter()
            .any(|l| l.contains("[rejected]") && l.contains("fetch first")),
        "{:?}",
        nff.2
    );
    // Every ref on the two servers is the same, with the same ids.
    let (git, w) = (&rig.git, rig.git.work("cmp"));
    let (c, p) = (
        ls_remote(git, &rig.asc("acme/web"), "2"),
        ls_remote(git, &rig.plain("acme/web"), "2"),
    );
    let names = |t: &str| {
        let mut v: Vec<String> = t.lines().map(str::to_owned).collect();
        v.sort();
        v
    };
    assert_eq!(names(&c), names(&p), "{w:?}");
}

#[test]
fn a_forced_push_and_a_mirror_push_reach_asc() {
    let rig = Rig::new();
    let git = &rig.git;
    let a = git.repo_with_commits("a", 2);
    let url = rig.asc("acme/web");
    git.ok(&a, &["remote", "add", "origin", &url]);
    assert!(git.run(&a, &["push", "origin", "master"]).ok);
    // A topic branch can be rewritten and force-pushed; a shared branch needs the policy to
    // allow it (U82), so the force on `master` is refused with a `remote:` reason.
    git.ok(&a, &["checkout", "-q", "-b", "topic"]);
    assert!(git.run(&a, &["push", "origin", "topic"]).ok);
    git.ok(&a, &["commit", "-q", "--amend", "-m", "rewritten"]);
    let forced = git.run(&a, &["push", "--force", "origin", "topic"]);
    let master_before = ls_remote(git, &url, "2");
    git.ok(&a, &["checkout", "-q", "master"]);
    git.ok(&a, &["reset", "-q", "--hard", "HEAD~1"]);
    let forced_master = git.run(&a, &["push", "--force", "origin", "master"]);
    assert!(
        forced.ok || forced.stderr.contains("asc:"),
        "a refused force says why: {}",
        mask(&forced.stderr)
    );
    assert!(
        !forced_master.ok && forced_master.stderr.contains("remote:"),
        "a forced rewrite of a shared ref is refused: {}",
        mask(&forced_master.stderr)
    );
    let master_after = ls_remote(git, &url, "2");
    let master_line = |t: &str| {
        t.lines()
            .find(|l| l.ends_with("refs/heads/master"))
            .map(str::to_owned)
    };
    assert_eq!(master_line(&master_before), master_line(&master_after));

    // Migration-style mirror push of a second repository with branches and tags.
    let m = git.repo_with_commits("m", 3);
    git.ok(&m, &["branch", "side"]);
    git.ok(&m, &["tag", "-a", "r1", "-m", "r"]);
    let lib = rig.asc("acme/lib");
    let r = git.run(&m, &["push", "--mirror", &lib]);
    assert!(r.ok, "{}", mask(&r.stderr));
    let c = git.work("mirror-clone");
    git.ok(&c, &["clone", "-q", "--mirror", &lib, "x.git"]);
    let x = c.join("x.git");
    assert_eq!(
        refs_of(git, &x),
        refs_of(git, &m)
            .into_iter()
            .filter(|l| !l.starts_with("refs/remotes"))
            .collect::<Vec<_>>()
    );
}

// =================================================================================== shallow, partial

#[test]
fn shallow_clones_and_partial_clones_match_a_plain_server() {
    let rig = Rig::new();
    let (git, scrub) = (&rig.git, rig.scrub());
    let src = fixture(git, "src", 30);
    seed(git, &src, &rig.asc("acme/web"));
    seed(git, &src, &rig.plain("acme/web"));
    let mut results: Vec<Vec<String>> = Vec::new();
    for (who, url) in [
        ("asc", rig.asc("acme/web")),
        ("plain", rig.plain("acme/web")),
    ] {
        let mut seen = Vec::new();
        let base = git.work(who);
        let r = git.run(&base, &["clone", "-q", "--depth", "1", &url, "s"]);
        assert!(r.ok, "{who}: depth: {}", mask(&r.stderr));
        let s = base.join("s");
        seen.push(git.ok(&s, &["rev-list", "--count", "HEAD"]));
        assert!(git.ok(&s, &["rev-parse", "--is-shallow-repository"]) == "true");
        for args in [
            vec!["fetch", "-q", "--deepen", "4"],
            vec!["fetch", "-q", "--shallow-since=2023-12-05"],
        ] {
            let r = git.run(&s, &args);
            assert!(r.ok, "{who}: {args:?}: {}", mask(&r.stderr));
            seen.push(git.ok(&s, &["rev-list", "--count", "--all"]));
        }
        let r = git.run(&s, &["fetch", "-q", "--unshallow"]);
        assert!(r.ok, "{who}: unshallow: {}", mask(&r.stderr));
        seen.push(git.ok(&s, &["rev-list", "--count", "--all"]));
        assert_eq!(
            git.ok(&s, &["rev-parse", "--is-shallow-repository"]),
            "false"
        );

        // Partial clones, and the lazy fetch of a missing blob.
        for (name, filter) in [
            ("blobless", "--filter=blob:none"),
            ("treeless", "--filter=tree:0"),
            ("limit", "--filter=blob:limit=1k"),
        ] {
            let r = git.run(&base, &["clone", "-q", filter, "--no-checkout", &url, name]);
            assert!(
                r.ok,
                "{who}: {filter}: {}",
                norm(&r.stderr, &scrub).join("|")
            );
            let w = base.join(name);
            let missing = git.ok(&w, &["rev-list", "--objects", "--missing=print", "--all"]);
            let missing = missing.lines().filter(|l| l.starts_with('?')).count();
            seen.push(format!("{name} missing {missing}"));
            assert!(
                name == "limit" || missing > 0 || name == "treeless",
                "{who}: {name} left nothing to fetch lazily"
            );
        }
        // The lazy fetch: a blob-less clone reads a file it does not have.
        let w = base.join("blobless");
        let blob = git.run(&w, &["cat-file", "-p", "HEAD:hot.txt"]);
        assert!(blob.ok, "{who}: lazy blob fetch: {}", mask(&blob.stderr));
        seen.push(blob.stdout);
        results.push(seen);
    }
    assert_eq!(
        results[0], results[1],
        "asc and a plain server agree on every count"
    );
}

#[test]
fn ls_remote_in_protocol_v0_v1_and_v2_matches_a_plain_server() {
    let rig = Rig::new();
    let git = &rig.git;
    let src = fixture(git, "src", 4);
    git.ok(&src, &["tag", "-a", "t1", "-m", "tag"]);
    git.ok(&src, &["tag", "light"]);
    git.ok(&src, &["branch", "side"]);
    seed(git, &src, &rig.asc("acme/web"));
    seed(git, &src, &rig.plain("acme/web"));
    for v in ["0", "1", "2"] {
        let (c, p) = (
            ls_remote(git, &rig.asc("acme/web"), v),
            ls_remote(git, &rig.plain("acme/web"), v),
        );
        let sorted = |t: &str| {
            let mut x: Vec<&str> = t.lines().collect();
            x.sort_unstable();
            x.join("\n")
        };
        assert_eq!(sorted(&c), sorted(&p), "protocol v{v}");
        assert!(c.contains("refs/tags/t1^{}"), "peeled tag in v{v}: {c}");
    }
}

#[test]
fn a_submodule_is_cloned_through_asc_urls() {
    let rig = Rig::new();
    let git = &rig.git;
    git.config("[protocol \"http\"]\n\tallow = always\n");
    let lib = git.repo_with_commits("lib", 2);
    seed(git, &lib, &rig.asc("acme/lib"));
    let sup = git.repo_with_commits("sup", 1);
    git.ok(&sup, &["remote", "add", "origin", &rig.asc("acme/web")]);
    git.ok(
        &sup,
        &["submodule", "add", "-q", &rig.asc("acme/lib"), "vendor/lib"],
    );
    // The URL in .gitmodules is relative to the superproject, so no credential is stored in it.
    git.ok(
        &sup,
        &[
            "config",
            "-f",
            ".gitmodules",
            "submodule.vendor/lib.url",
            "../lib.git",
        ],
    );
    git.ok(&sup, &["add", ".gitmodules"]);
    git.ok(&sup, &["commit", "-q", "-m", "submodule"]);
    assert!(git.run(&sup, &["push", "origin", "master"]).ok);

    let base = git.work("clone");
    let r = git.run(
        &base,
        &[
            "clone",
            "-q",
            "--recurse-submodules",
            &rig.asc("acme/web"),
            "w",
        ],
    );
    assert!(r.ok, "{}", mask(&r.stderr));
    assert_eq!(
        git.head(&base.join("w").join("vendor/lib")),
        git.head(&lib),
        "the submodule is at the pinned commit"
    );
}

// =================================================================================== credentials

/// The U79 credential paths, repeated against the live git layer. Credentials never appear in
/// an assertion message (`mask`).
mod credentials {
    use super::*;

    /// A helper that records each call (`get`, `store`, `erase`) and its input, and answers `get`
    /// with the token it reads from a file (so no secret is in the configuration).
    struct Helper {
        dir: PathBuf,
    }

    impl Helper {
        fn new(git: &Git, name: &str, token: &str) -> Self {
            let dir = git.work(name);
            std::fs::write(dir.join("secret"), token).unwrap();
            std::fs::write(
                dir.join("h.sh"),
                format!(
                    "#!/bin/sh\nD='{d}'\nop=\"$1\"\necho \"$op\" >> \"$D/ops\"\ncat > \"$D/$op.in\"\n\
                     if [ \"$op\" = get ]; then\n  echo username=x-access-token\n  echo \"password=$(cat \"$D/secret\")\"\nfi\n",
                    d = fwd(&dir)
                ),
            )
            .unwrap();
            Self { dir }
        }

        fn spec(&self) -> String {
            format!("!sh {}", fwd(&self.dir.join("h.sh")))
        }

        fn ops(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.join("ops"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        fn input(&self, op: &str) -> String {
            std::fs::read_to_string(self.dir.join(format!("{op}.in"))).unwrap_or_default()
        }
    }

    fn ls(git: &Git, url: &str, env: &[(&str, &str)]) -> Run {
        git.run_env(&git.work("ls"), &["ls-remote", url], env)
    }

    #[test]
    fn a_helper_gets_the_challenge_and_stores_after_success_and_erases_after_a_revoked_token() {
        let rig = Rig::new();
        let git = &rig.git;
        let h = Helper::new(git, "h1", &rig.write.secret);
        git.config(&format!("[credential]\n\thelper = {}\n", h.spec()));
        let r = ls(git, &rig.stack.url("acme/web"), &[]);
        assert!(r.ok, "{}", mask(&r.stderr));
        assert_eq!(h.ops(), ["get", "store"]);
        assert!(
            h.input("get").contains("wwwauth[]=Basic"),
            "{}",
            mask(&h.input("get"))
        );
        assert!(h.input("get").contains("host=127.0.0.1:"));
        assert!(h.input("store").contains("username=x-access-token"));

        // A revoked token: 401, and the helper is told to erase it.
        let t = rig
            .stack
            .token(&["--user", "carol", "--repo", "acme/*", "--write"]);
        let h2 = Helper::new(git, "h2", &t.secret);
        rig.stack.revoke(&t);
        git.config(&format!(
            "[credential \"{}\"]\n\thelper = \n\thelper = {}\n",
            "http://127.0.0.1:1",
            h2.spec()
        ));
        let r = git.run_env(
            &git.work("ls2"),
            &[
                "-c",
                "credential.helper=",
                "-c",
                &format!("credential.helper={}", h2.spec()),
                "ls-remote",
                &rig.stack.url("acme/web"),
            ],
            &[],
        );
        assert!(!r.ok, "a revoked token is refused");
        assert!(
            r.stderr.contains("Authentication failed") || r.stderr.contains("401"),
            "{}",
            mask(&r.stderr)
        );
        assert_eq!(h2.ops(), ["get", "erase"], "{}", mask(&r.stderr));
    }

    #[test]
    fn a_url_scoped_helper_use_http_path_and_credential_username() {
        let rig = Rig::new();
        let git = &rig.git;
        let h = Helper::new(git, "h", &rig.write.secret);
        git.config(&format!(
            "[credential \"http://{host}\"]\n\thelper = {h}\n\tuseHttpPath = true\n\tusername = x-access-token\n",
            host = rig.stack.host(),
            h = h.spec()
        ));
        let r = ls(git, &rig.stack.url("acme/web"), &[]);
        assert!(r.ok, "{}", mask(&r.stderr));
        let input = h.input("get");
        assert!(input.contains("path=acme/web.git"), "{}", mask(&input));
        assert!(
            input.contains("username=x-access-token"),
            "{}",
            mask(&input)
        );
        // A helper scoped to another host is never asked.
        let other = Helper::new(git, "other", "unused");
        git.config(&format!(
            "[credential \"http://other.invalid\"]\n\thelper = {}\n",
            other.spec()
        ));
        assert!(ls(git, &rig.stack.url("acme/web"), &[]).ok);
        assert!(
            other.ops().is_empty(),
            "a helper for another host is not called"
        );
    }

    #[test]
    fn the_store_helper_reads_its_file() {
        let rig = Rig::new();
        let git = &rig.git;
        let file = git.work("st").join("creds");
        std::fs::write(
            &file,
            format!(
                "http://x-access-token:{}@{}\n",
                rig.write.secret,
                rig.stack.host()
            ),
        )
        .unwrap();
        git.config(&format!(
            "[credential]\n\thelper = store --file={}\n",
            fwd(&file)
        ));
        let r = ls(git, &rig.stack.url("acme/web"), &[]);
        assert!(r.ok, "{}", mask(&r.stderr));
    }

    #[cfg(unix)]
    #[test]
    fn the_cache_helper_keeps_the_credential_between_runs() {
        let rig = Rig::new();
        let git = &rig.git;
        let h = Helper::new(git, "h", &rig.write.secret);
        git.config(&format!(
            "[credential]\n\thelper = cache --timeout=30\n\thelper = {}\n",
            h.spec()
        ));
        assert!(ls(git, &rig.stack.url("acme/web"), &[]).ok);
        assert!(ls(git, &rig.stack.url("acme/web"), &[]).ok);
        assert_eq!(
            h.ops().iter().filter(|o| *o == "get").count(),
            1,
            "the second run hit the cache"
        );
    }

    /// The GCM case: only the Windows credential manager, which CI does not have.
    #[test]
    #[ignore = "needs Git Credential Manager on Windows"]
    fn git_credential_manager_receives_the_challenge() {
        let rig = Rig::new();
        let r = ls(&rig.git, &rig.stack.url("acme/web"), &[]);
        assert!(r.ok, "{}", mask(&r.stderr));
    }

    #[test]
    fn netrc_extra_header_askpass_and_userinfo_authenticate() {
        let rig = Rig::new();
        let git = &rig.git;
        let url = rig.stack.url("acme/web");
        let host = rig.stack.host();
        let secret = &rig.write.secret;

        // .netrc (and _netrc, which libcurl reads on Windows).
        let netrc = format!("machine 127.0.0.1 login x-access-token password {secret}\n");
        for n in [".netrc", "_netrc"] {
            std::fs::write(git.home.path().join(n), &netrc).unwrap();
        }
        let r = ls(git, &url, &[]);
        assert!(r.ok, ".netrc: {}", mask(&r.stderr));
        for n in [".netrc", "_netrc"] {
            std::fs::remove_file(git.home.path().join(n)).unwrap();
        }
        let r = ls(git, &url, &[]);
        assert!(!r.ok, "without a credential source the request fails");

        // http.extraHeader as actions/checkout writes it, and the Azure DevOps bearer style.
        let basic_value = b64(format!("x-access-token:{secret}").as_bytes());
        let r = git.run(
            &git.work("eh"),
            &[
                "-c",
                &format!("http.http://{host}/.extraheader=AUTHORIZATION: basic {basic_value}"),
                "ls-remote",
                &url,
            ],
        );
        assert!(r.ok, "extraheader (basic): {}", mask(&r.stderr));
        let r = git.run(
            &git.work("eh2"),
            &[
                "-c",
                &format!("http.extraHeader=Authorization: Bearer {secret}"),
                "ls-remote",
                &url,
            ],
        );
        assert!(r.ok, "extraHeader (bearer): {}", mask(&r.stderr));

        // GIT_ASKPASS answers the username and password prompts.
        let ask = git.work("ask");
        std::fs::write(ask.join("secret"), secret).unwrap();
        std::fs::write(
            ask.join("ask.sh"),
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  Username*) echo x-access-token;;\n  Password*) cat '{}';;\nesac\n",
                fwd(&ask.join("secret"))
            ),
        )
        .unwrap();
        make_executable(&ask.join("ask.sh"));
        let ask_path = fwd(&ask.join("ask.sh"));
        let r = ls(git, &url, &[("GIT_ASKPASS", &ask_path)]);
        assert!(r.ok, "askpass: {}", mask(&r.stderr));

        // https://TOKEN@host: the token as the user name, the password from askpass (empty).
        std::fs::write(
            ask.join("empty.sh"),
            "#!/bin/sh\ncase \"$1\" in\n  Password*) echo '';;\nesac\n",
        )
        .unwrap();
        make_executable(&ask.join("empty.sh"));
        let token_only = format!("http://{secret}@{host}/acme/web.git");
        let r = ls(
            git,
            &token_only,
            &[("GIT_ASKPASS", &fwd(&ask.join("empty.sh")))],
        );
        assert!(r.ok, "token as the user name: {}", mask(&r.stderr));

        // GIT_TERMINAL_PROMPT=0 with nothing to answer: a clean failure, no hang.
        let r = ls(git, &url, &[]);
        assert!(!r.ok);
        assert!(
            r.stderr.contains("terminal prompts disabled")
                || r.stderr.contains("Authentication failed"),
            "{}",
            mask(&r.stderr)
        );
    }

    #[test]
    fn deploy_tokens_read_and_write_are_told_apart_and_ci_fetches_one_sha() {
        let rig = Rig::new();
        let git = &rig.git;
        let rd = rig
            .stack
            .token(&["--deploy", "--repo", "acme/web", "--read"]);
        let wr = rig
            .stack
            .token(&["--deploy", "--repo", "acme/web", "--write"]);
        let src = git.repo_with_commits("src", 2);
        let sha = git.head(&src);
        let r = git.run(
            &src,
            &["push", &rig.stack.url_with("acme/web", &wr), "master"],
        );
        assert!(r.ok, "write deploy token pushes: {}", mask(&r.stderr));
        let r = git.run(
            &src,
            &[
                "push",
                &rig.stack.url_with("acme/web", &rd),
                "HEAD:refs/heads/more",
            ],
        );
        assert!(!r.ok, "a read deploy token cannot push");
        assert!(
            r.stderr.contains("403")
                || r.stderr.contains("denied")
                || r.stderr.contains("Authentication"),
            "{}",
            mask(&r.stderr)
        );
        let c = git.run(
            &git.work("rdclone"),
            &["clone", "-q", &rig.stack.url_with("acme/web", &rd), "w"],
        );
        assert!(c.ok, "a read deploy token clones: {}", mask(&c.stderr));
        // A deploy token reaches its one repository only.
        let other = git.run(
            &git.work("x"),
            &["ls-remote", &rig.stack.url_with("acme/lib", &rd)],
        );
        assert!(
            !other.ok,
            "a deploy token for acme/web does not read acme/lib"
        );

        // actions/checkout: init, an extraheader from the token, a shallow fetch of one SHA.
        let w = git.work("ci");
        git.ok(&w, &["init", "-q"]);
        git.ok(&w, &["remote", "add", "origin", &rig.stack.url("acme/web")]);
        let header = format!(
            "AUTHORIZATION: basic {}",
            b64(format!("x-access-token:{}", rd.secret).as_bytes())
        );
        let key = format!("http.http://{}/.extraheader", rig.stack.host());
        let r = git.run(
            &w,
            &[
                "-c",
                &format!("{key}={header}"),
                "fetch",
                "--no-tags",
                "--prune",
                "--no-recurse-submodules",
                "--depth=1",
                "origin",
                &format!("+{sha}:refs/remotes/origin/master"),
            ],
        );
        assert!(r.ok, "fetch of one SHA: {}", mask(&r.stderr));
        git.ok(
            &w,
            &["checkout", "-q", "--force", "refs/remotes/origin/master"],
        );
        assert_eq!(git.head(&w), sha);
    }
}

// =================================================================================== security

mod security {
    use super::*;

    fn auth(t: &Token) -> String {
        basic("x-access-token", &t.secret)
    }

    #[test]
    fn paths_services_and_unreadable_repos_follow_the_rules() {
        let rig = Rig::new();
        let addr = rig.stack.addr();
        let a = auth(&rig.write);
        let h = [("Authorization", a.as_str())];
        let get = |path: &str| http(addr, "GET", path, &h, b"");

        // The three routes answer; anything else on a git path does not.
        assert_eq!(
            get("/acme/web.git/info/refs?service=git-upload-pack").status,
            200
        );
        for bad in [
            "/acme/web.git/HEAD",
            "/acme/web.git/objects/info/packs",
            "/acme/web.git/info/refs/",
            "/acme/web.git/info/refs/extra?service=git-upload-pack",
            "/acme/%2e%2e/web.git/info/refs?service=git-upload-pack",
            "/acme/web%2f.git/info/refs?service=git-upload-pack",
            "/acme/web.git/info/refs%3Fservice=git-upload-pack",
            "/acme/.hidden.git/info/refs?service=git-upload-pack",
            "/acme//web.git/info/refs?service=git-upload-pack",
            "/acme/web.git/info/%72efs?service=git-upload-pack",
        ] {
            let s = get(bad).status;
            assert!(s == 404 || s == 400 || s == 403, "{bad} answered {s}");
        }
        for service in ["git-upload-archive", "nonsense", ""] {
            let s = get(&format!("/acme/web.git/info/refs?service={service}")).status;
            assert!(
                s == 403 || s == 404 || s == 400,
                "service {service:?} answered {s}"
            );
        }
        // The dumb protocol (no `service=`) is not served.
        let dumb = get("/acme/web.git/info/refs").status;
        assert!(dumb == 403 || dumb == 404, "dumb info/refs answered {dumb}");

        // Anonymous: a Basic challenge (git's flow). A wrong token: the same challenge.
        let anon = http(
            addr,
            "GET",
            "/acme/web.git/info/refs?service=git-upload-pack",
            &[],
            b"",
        );
        assert_eq!(anon.status, 401);
        assert!(
            anon.header("www-authenticate")
                .is_some_and(|v| v.starts_with("Basic"))
        );
        let wrong = basic("x-access-token", "not-a-token-0123456789abcdef");
        let r = http(
            addr,
            "GET",
            "/acme/web.git/info/refs?service=git-upload-pack",
            &[("Authorization", &wrong)],
            b"",
        );
        assert_eq!(r.status, 401);
        assert!(
            !r.text().contains("not-a-token"),
            "the body repeats no credential"
        );

        // A valid token that reads nothing here: 404, as for a repo that does not exist.
        let lonely = rig
            .stack
            .token(&["--user", "dave", "--repo", "other/*", "--read"]);
        let la = auth(&lonely);
        let hidden = http(
            addr,
            "GET",
            "/acme/web.git/info/refs?service=git-upload-pack",
            &[("Authorization", &la)],
            b"",
        );
        let missing = http(
            addr,
            "GET",
            "/acme/nope.git/info/refs?service=git-upload-pack",
            &[("Authorization", &la)],
            b"",
        );
        assert_eq!(hidden.status, 404);
        assert_eq!(hidden.status, missing.status);
        assert_eq!(
            hidden.text(),
            missing.text(),
            "unreadable and absent look the same"
        );

        // A read token may fetch but not push: the receive-pack advertisement is refused.
        let ra = auth(&rig.read);
        let push = http(
            addr,
            "GET",
            "/acme/web.git/info/refs?service=git-receive-pack",
            &[("Authorization", &ra)],
            b"",
        );
        assert_eq!(push.status, 403);
        assert!(!push.text().contains(&rig.read.secret));
    }

    #[test]
    fn the_server_log_and_metrics_never_hold_a_credential_and_each_request_is_counted() {
        let rig = Rig::new();
        let git = &rig.git;
        let src = git.repo_with_commits("src", 1);
        assert!(git.run(&src, &["push", &rig.asc("acme/web"), "master"]).ok);
        let before = metric_total(&rig);
        // Three requests of different kinds: an allowed discovery, a refused push discovery
        // and an anonymous one. Each is one record, whatever its answer.
        let w = format!(
            "Basic {}",
            b64(format!("x-access-token:{}", rig.write.secret).as_bytes())
        );
        let r = format!(
            "Basic {}",
            b64(format!("x-access-token:{}", rig.read.secret).as_bytes())
        );
        let info = "/acme/web.git/info/refs?service=git-upload-pack";
        let push = "/acme/web.git/info/refs?service=git-receive-pack";
        let addr = rig.stack.addr();
        assert_eq!(
            http(addr, "GET", info, &[("Authorization", &w)], b"").status,
            200
        );
        assert_eq!(
            http(addr, "GET", push, &[("Authorization", &r)], b"").status,
            403
        );
        assert_eq!(http(addr, "GET", info, &[], b"").status, 401);
        let after = metric_total(&rig);
        assert_eq!(after - before, 3, "one metric and audit record per request");
        let admin = std::net::SocketAddr::from(([127, 0, 0, 1], rig.stack.admin_port));
        let metrics = http(admin, "GET", "/metrics", &[], b"").text();
        let log = rig.stack.log();
        for secret in [&rig.write.secret, &rig.read.secret, &rig.stack.admin.secret] {
            assert!(
                !metrics.contains(secret.as_str()),
                "metrics hold a credential"
            );
            assert!(
                !log.contains(secret.as_str()),
                "the server log holds a credential"
            );
        }
    }

    fn metric_total(rig: &Rig) -> u64 {
        let admin = std::net::SocketAddr::from(([127, 0, 0, 1], rig.stack.admin_port));
        http(admin, "GET", "/metrics", &[], b"")
            .text()
            .lines()
            .filter(|l| l.starts_with("asc_git_requests_total"))
            .filter_map(|l| l.rsplit(' ').next()?.parse::<u64>().ok())
            .sum()
    }

    #[test]
    fn disabled_at_run_time_gives_the_stub_to_git_and_to_raw_requests() {
        let stack = Stack::start_with("enabled = false\n", "", "");
        stack.create_repo("acme/web", "");
        let write = stack.token(&["--user", "alice", "--repo", "acme/*", "--write"]);
        let a = basic("x-access-token", &write.secret);
        for (method, path) in [
            ("GET", "/acme/web.git/info/refs?service=git-upload-pack"),
            ("POST", "/acme/web.git/git-upload-pack"),
            ("POST", "/acme/web.git/git-receive-pack"),
        ] {
            let anon = http(stack.addr(), method, path, &[], b"0000");
            assert_eq!(anon.status, 401, "{path}");
            assert!(
                anon.header("www-authenticate")
                    .is_some_and(|v| v.starts_with("Basic"))
            );
            let known = http(
                stack.addr(),
                method,
                path,
                &[("Authorization", &a)],
                b"0000",
            );
            assert_eq!(known.status, 403, "{path}");
            assert_eq!(known.header("x-asc-git"), Some("disabled"));
            assert!(
                known.text().contains("git protocol disabled"),
                "{}",
                known.text()
            );
        }
        // Git sees the same: the challenge, the helper-free retry with the URL's credential, 403.
        let git = Git::new();
        let r = git.run(
            &git.work("c"),
            &["clone", &stack.url_with("acme/web", &write), "w"],
        );
        assert!(!r.ok);
        assert!(r.stderr.contains("403"), "{}", mask(&r.stderr));
        // The rest of the server works (the switch removes the layer, nothing else).
        assert_eq!(http(stack.addr(), "GET", "/healthz", &[], b"").status, 200);
    }

    #[test]
    fn a_fetch_over_the_per_principal_limit_is_shed() {
        let stack = Stack::start_with("enabled = true\n", "fetch_per_principal = 1\n", "");
        stack.create_repo("acme/web", "");
        let write = stack.token(&["--user", "alice", "--repo", "acme/*", "--write"]);
        let git = Git::new();
        let src = git.repo_with_commits("src", 1);
        assert!(
            git.run(
                &src,
                &["push", &stack.url_with("acme/web", &write), "master"]
            )
            .ok
        );

        // The first fetch POST holds its slot: headers and a few body bytes, then silence.
        use std::io::Write;
        let a = basic("x-access-token", &write.secret);
        let mut held = std::net::TcpStream::connect(stack.addr()).unwrap();
        write!(
            held,
            "POST /acme/web.git/git-upload-pack HTTP/1.1\r\nHost: x\r\nAuthorization: {a}\r\n\
             Content-Type: application/x-git-upload-pack-request\r\nContent-Length: 4096\r\n\r\n0032"
        )
        .unwrap();
        held.flush().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let shed = http(
            stack.addr(),
            "POST",
            "/acme/web.git/git-upload-pack",
            &[
                ("Authorization", &a),
                ("Content-Type", "application/x-git-upload-pack-request"),
            ],
            b"0032want 0000000000000000000000000000000000000000\n0000",
        );
        assert!(
            shed.status == 429 || shed.status == 503,
            "the second concurrent fetch is shed, got {}",
            shed.status
        );
        assert!(shed.header("retry-after").is_some() || shed.status == 429);
        drop(held);
    }

    #[test]
    fn a_mirror_repository_is_read_only_over_git() {
        let stack = Stack::start();
        // `mode = "mirror"` needs the mirror service; a server without it refuses the init.
        stack.create_repo(
            "acme/web",
            "\"mode\":\"mirror\",\"defaultBranch\":\"trunk\"",
        );
        let write = stack.token(&["--user", "alice", "--repo", "acme/*", "--write"]);
        let a = basic("x-access-token", &write.secret);
        let push = http(
            stack.addr(),
            "GET",
            "/acme/web.git/info/refs?service=git-receive-pack",
            &[("Authorization", &a)],
            b"",
        );
        assert_eq!(push.status, 403, "{}", push.text());
        assert!(push.text().contains("read-only mirror"), "{}", push.text());
    }
}
