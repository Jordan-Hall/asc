//! `asc cache go` as the go command's `GOCACHEPROG`: a module built in one work tree is not
//! compiled again in another work tree with the same sources (with `-trimpath`), the binary it
//! links runs, and a changed source compiles again.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use support::{Sandbox, asc_binary, text};

fn ok(sb: &Sandbox, cwd: &Path, args: &[&str]) -> String {
    let out = sb.asc(cwd).args(args).output().expect("spawn asc");
    assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
    text(&out.stdout)
}

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// A module: `main` uses package `greet`.
fn module(sb: &Sandbox, name: &str) -> PathBuf {
    let repo = sb.dir(name);
    ok(sb, &repo, &["init", "-q"]);
    ok(sb, &repo, &["config", "asc.buildcache.remote", "off"]);
    write(&repo, "go.mod", "module example.org/hello\n\ngo 1.22\n");
    write(
        &repo,
        "greet/greet.go",
        "package greet\n\nfunc Hello() string { return \"hi from greet\" }\n",
    );
    write(
        &repo,
        "main.go",
        "package main\n\nimport (\n\t\"fmt\"\n\n\t\"example.org/hello/greet\"\n)\n\nfunc main() { fmt.Println(greet.Hello()) }\n",
    );
    repo
}

fn go_present() -> bool {
    Command::new("go")
        .arg("version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn go(sb: &Sandbox, repo: &Path, args: &[&str]) -> Output {
    let prog = asc_binary().unwrap().replace('\\', "/");
    let mut cmd = sb.command("go", repo);
    cmd.env("GOCACHEPROG", format!("\"{prog}\" cache go"))
        .env("GOFLAGS", "-trimpath")
        .env("GOCACHE", sb.dir("gocache"))
        .env("GOPATH", sb.dir("gopath"))
        .env("GOENV", "off")
        .env("GOTOOLCHAIN", "local")
        .env("GOPROXY", "off")
        .env("GOWORK", "off")
        .env_remove("CI")
        .args(args);
    cmd.output().expect("spawn go")
}

fn go_entries(sb: &Sandbox, repo: &Path) -> usize {
    let v: Value =
        serde_json::from_str(&ok(sb, repo, &["cache", "ls", "--tool", "go", "--json"])).unwrap();
    v.as_array().unwrap().len()
}

fn hits(sb: &Sandbox, repo: &Path) -> u64 {
    let v: Value = serde_json::from_str(&ok(sb, repo, &["cache", "status", "--json"])).unwrap();
    v["tools"]["go"]["day"]["hits"].as_u64().unwrap_or(0)
}

fn hello(repo: &Path) -> String {
    let exe = repo.join(format!("hello{}", std::env::consts::EXE_SUFFIX));
    text(&Command::new(exe).output().expect("run hello").stdout)
}

#[test]
fn a_second_work_tree_reuses_the_go_build() {
    if asc_binary().is_none() || !go_present() {
        return;
    }
    let sb = Sandbox::new();
    let a = module(&sb, "a");
    let exe = format!("hello{}", std::env::consts::EXE_SUFFIX);
    let built = go(&sb, &a, &["build", "-o", &exe, "."]);
    assert!(built.status.success(), "{}", text(&built.stderr));
    assert_eq!(hello(&a).trim(), "hi from greet");
    let first = go_entries(&sb, &a);
    assert!(
        first > 2,
        "the standard library, greet, main and the link: {first}"
    );

    let b = module(&sb, "b");
    let before = hits(&sb, &b);
    let again = go(&sb, &b, &["build", "-o", &exe, "."]);
    assert!(again.status.success(), "{}", text(&again.stderr));
    assert_eq!(hello(&b).trim(), "hi from greet");
    assert!(hits(&sb, &b) > before, "the second work tree hits");
    assert_eq!(go_entries(&sb, &b), first, "nothing new was stored");

    write(
        &b,
        "greet/greet.go",
        "package greet\n\nfunc Hello() string { return \"changed\" }\n",
    );
    let changed = go(&sb, &b, &["build", "-o", &exe, "."]);
    assert!(changed.status.success(), "{}", text(&changed.stderr));
    assert_eq!(hello(&b).trim(), "changed");
    assert!(go_entries(&sb, &b) > first);
}

#[test]
fn setup_go_writes_and_undoes_the_go_env_file() {
    if asc_binary().is_none() || !go_present() {
        return;
    }
    let sb = Sandbox::new();
    let a = module(&sb, "a");
    let env_file = sb.dir("goenv").join("env");
    let setup = |args: &[&str]| {
        let out = sb
            .asc(&a)
            .env("GOENV", &env_file)
            .env_remove("GOCACHEPROG")
            .env_remove("GOFLAGS")
            .args(["cache", "setup", "go"])
            .args(args)
            .output()
            .expect("spawn asc");
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    };
    let said = setup(&["--trimpath"]);
    assert!(
        said.contains("set GOCACHEPROG = ") && said.contains("cache go"),
        "{said}"
    );
    let file = std::fs::read_to_string(&env_file).unwrap();
    assert!(
        file.contains("GOCACHEPROG=") && file.contains("GOFLAGS=-trimpath"),
        "{file}"
    );
    assert!(setup(&["--trimpath"]).contains("nothing to change"));
    let undone = setup(&["--undo", "--trimpath"]);
    assert!(
        undone.contains("removed GOCACHEPROG") && undone.contains("removed -trimpath"),
        "{undone}"
    );
    let file = std::fs::read_to_string(&env_file).unwrap_or_default();
    assert!(
        !file.contains("GOCACHEPROG") && !file.contains("GOFLAGS"),
        "{file}"
    );
}
