//! B01 acceptance: the branchless keys and the effective mode, seen through `asc create`.
//! `asc create` names no branch in branchless mode (the default) and names one from
//! `asc.stack.branchTemplate` when `asc.branchless` is off, at every scope that can set it. The
//! pure rules (mode AND, cloud on a public repo, push style) are unit-tested in
//! `src/branchless/mode.rs`.

mod support;

use std::fs;
use std::path::PathBuf;

use support::{Sandbox, text};

struct Repo {
    sb: Sandbox,
    root: PathBuf,
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Repo {
    fn new() -> Self {
        let sb = Sandbox::new();
        fs::write(
            sb.user.join(".gitconfig"),
            "[user]\n\tname = Ada Lovelace\n\temail = ada@example.com\n",
        )
        .unwrap();
        let root = sb.dir("repo");
        let r = Self { sb, root };
        r.ok(&[], &[], "init");
        fs::write(r.root.join("f"), "a\n").unwrap();
        r.ok(&[], &[], "add f");
        r.ok(&[], &[], "commit -m base");
        r
    }

    fn run(&self, env: &[(&str, &str)], globals: &[&str], args: &str) -> Out {
        let mut cmd = self.sb.asc(&self.root);
        for g in globals {
            cmd.args(["-c", g]);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd
            .args(args.split(' '))
            .stdin(std::process::Stdio::null())
            .output()
            .expect("spawn");
        Out {
            code: out.status.code().unwrap_or(-1),
            stdout: text(&out.stdout),
            stderr: text(&out.stderr),
        }
    }

    fn ok(&self, env: &[(&str, &str)], globals: &[&str], args: &str) -> String {
        let o = self.run(env, globals, args);
        assert_eq!(o.code, 0, "asc {args}: {}", o.stderr);
        o.stdout
    }

    fn branches(&self) -> Vec<String> {
        self.ok(&[], &[], "branch --format=%(refname:short)")
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn current(&self) -> String {
        self.ok(&[], &[], "branch --show-current").trim().to_owned()
    }

    fn stage(&self, name: &str) {
        fs::write(self.root.join(name), name).unwrap();
        self.ok(&[], &[], &format!("add {name}"));
    }
}

#[test]
fn create_names_no_branch_by_default() {
    let r = Repo::new();
    let start = r.current();
    r.stage("one");
    r.ok(&[], &[], "create -m feat:one");
    assert_eq!(r.current(), start);
    assert_eq!(r.branches(), [start]);
}

#[test]
fn create_names_a_branch_when_one_is_given() {
    let r = Repo::new();
    r.stage("one");
    r.ok(&[], &[], "create topic -m feat:one");
    assert_eq!(r.current(), "topic");
}

#[test]
fn branchless_off_names_a_branch_from_the_template_at_every_scope() {
    // Local config (the repo's common config).
    let r = Repo::new();
    r.ok(&[], &[], "config asc.branchless false");
    r.stage("one");
    r.ok(&[], &[], "create -m Add-the-thing");
    assert_eq!(r.current(), "ada-lovelace/add-the-thing");

    // A custom template.
    r.ok(&[], &[], "config asc.stack.branchTemplate wip/{slug}");
    r.stage("two");
    r.ok(&[], &[], "create -m second");
    assert_eq!(r.current(), "wip/second");

    // `--no-branch` suppresses it.
    r.stage("three");
    r.ok(&[], &[], "create --no-branch -m third");
    assert_eq!(r.current(), "wip/second");

    // Global config.
    let g = Repo::new();
    let mut cfg = fs::read_to_string(g.sb.user.join(".gitconfig")).unwrap();
    cfg.push_str("[asc]\n\tbranchless = false\n");
    fs::write(g.sb.user.join(".gitconfig"), cfg).unwrap();
    g.stage("one");
    g.ok(&[], &[], "create -m one");
    assert_eq!(g.current(), "ada-lovelace/one");

    // System config.
    let s = Repo::new();
    let sys = s.sb.dir("sys").join("gitconfig");
    fs::write(&sys, "[asc]\n\tbranchless = false\n").unwrap();
    s.stage("one");
    let o = s.run(
        &[
            ("GIT_CONFIG_NOSYSTEM", "0"),
            ("GIT_CONFIG_SYSTEM", sys.to_str().unwrap()),
        ],
        &[],
        "create -m sys",
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(s.current(), "ada-lovelace/sys");

    // `-c` on the command line.
    let c = Repo::new();
    c.stage("one");
    c.ok(&[], &["asc.branchless=false"], "create -m dash-c");
    assert_eq!(c.current(), "ada-lovelace/dash-c");

    // The environment variable.
    let e = Repo::new();
    e.stage("one");
    e.ok(&[("ASC_BRANCHLESS", "false")], &[], "create -m from-env");
    assert_eq!(e.current(), "ada-lovelace/from-env");
}

#[test]
fn env_and_dash_c_order() {
    let r = Repo::new();
    r.ok(&[], &[], "config asc.branchless false");
    let start = r.current();
    // The environment beats the files.
    r.stage("one");
    r.ok(&[("ASC_BRANCHLESS", "true")], &[], "create -m one");
    assert_eq!(r.current(), start);
    // `-c` beats the environment.
    r.stage("two");
    r.ok(
        &[("ASC_BRANCHLESS", "true")],
        &["asc.branchless=false"],
        "create -m two",
    );
    assert_eq!(r.current(), "ada-lovelace/two");
}

#[test]
fn a_bad_switch_is_refused_and_changes_nothing() {
    let r = Repo::new();
    let start = r.current();
    r.stage("one");
    let before = r.ok(&[], &[], "rev-parse HEAD");
    let o = r.run(&[("ASC_BRANCHLESS", "maybe")], &[], "create -m one");
    assert_ne!(o.code, 0);
    assert!(o.stderr.contains("ASC_BRANCHLESS"), "{}", o.stderr);
    assert!(o.stdout.is_empty());
    assert_eq!(r.ok(&[], &[], "rev-parse HEAD"), before);
    assert_eq!(r.current(), start);
}
