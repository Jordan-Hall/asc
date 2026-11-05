//! `asc` as cargo's `RUSTC_WRAPPER`: a workspace built in one work tree is restored, not
//! compiled, in another work tree with the same sources, and the binary it links runs; a changed
//! source compiles that crate and the crates above it again.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use support::{Sandbox, asc_binary, text};

const IDENT: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
];

fn ok(sb: &Sandbox, cwd: &Path, args: &[&str]) -> String {
    let out = sb
        .asc(cwd)
        .envs(IDENT)
        .args(args)
        .output()
        .expect("spawn asc");
    assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
    text(&out.stdout)
}

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// A committed workspace: `app` (a binary) uses `util` (a library).
fn workspace(sb: &Sandbox, name: &str) -> PathBuf {
    let repo = sb.dir(name);
    ok(sb, &repo, &["init", "-q"]);
    ok(sb, &repo, &["config", "asc.buildcache.remote", "off"]);
    write(&repo, ".gitignore", "target/\n");
    write(
        &repo,
        "Cargo.toml",
        "[workspace]\nmembers = [\"app\", \"util\"]\nresolver = \"2\"\n",
    );
    write(
        &repo,
        "util/Cargo.toml",
        "[package]\nname = \"util\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        &repo,
        "util/src/lib.rs",
        "pub fn greet() -> &'static str {\n    \"hi from util\"\n}\n",
    );
    write(
        &repo,
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nutil = { path = \"../util\" }\n",
    );
    write(
        &repo,
        "app/src/main.rs",
        "fn main() {\n    println!(\"{} {}\", util::greet(), env!(\"CARGO_PKG_NAME\"));\n}\n",
    );
    ok(sb, &repo, &["add", "."]);
    ok(sb, &repo, &["commit", "-q", "-m", "base"]);
    repo
}

/// The real cargo and toolchain homes: the sandbox moves `HOME`.
fn homes() -> [(&'static str, PathBuf); 2] {
    let home = std::env::home_dir().unwrap_or_default();
    let get = |var: &str, dir: &str| {
        std::env::var_os(var)
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(dir))
    };
    [
        ("CARGO_HOME", get("CARGO_HOME", ".cargo")),
        ("RUSTUP_HOME", get("RUSTUP_HOME", ".rustup")),
    ]
}

fn cargo(sb: &Sandbox, repo: &Path, args: &[&str]) -> Output {
    let mut cmd = sb.command("cargo", repo);
    cmd.envs(homes())
        .env("RUSTC_WRAPPER", asc_binary().unwrap())
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_TARGET_DIR", repo.join("target"))
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("CARGO_BUILD_RUSTC_WRAPPER")
        .env_remove("CI")
        .args(args);
    cmd.output().expect("spawn cargo")
}

fn rustc_entries(sb: &Sandbox, repo: &Path) -> usize {
    let v: Value =
        serde_json::from_str(&ok(sb, repo, &["cache", "ls", "--tool", "rustc", "--json"])).unwrap();
    v.as_array().unwrap().len()
}

fn hits(sb: &Sandbox, repo: &Path) -> u64 {
    let v: Value = serde_json::from_str(&ok(sb, repo, &["cache", "status", "--json"])).unwrap();
    v["tools"]["rustc"]["day"]["hits"].as_u64().unwrap_or(0)
}

fn app(repo: &Path) -> String {
    let exe = repo
        .join("target/debug")
        .join(format!("app{}", std::env::consts::EXE_SUFFIX));
    text(&Command::new(exe).output().expect("run app").stdout)
}

#[test]
fn a_second_work_tree_restores_the_build_instead_of_compiling() {
    if asc_binary().is_none() || Command::new("cargo").arg("-V").output().is_err() {
        return;
    }
    let sb = Sandbox::new();
    let a = workspace(&sb, "a");
    let built = cargo(&sb, &a, &["build", "-q"]);
    assert!(built.status.success(), "{}", text(&built.stderr));
    assert_eq!(app(&a).trim(), "hi from util app");
    assert_eq!(rustc_entries(&sb, &a), 2, "util and app");
    assert_eq!(hits(&sb, &a), 0);

    let b = workspace(&sb, "b");
    let again = cargo(&sb, &b, &["build", "-q"]);
    assert!(again.status.success(), "{}", text(&again.stderr));
    assert_eq!(hits(&sb, &b), 2, "both crates restored");
    assert_eq!(rustc_entries(&sb, &b), 2);
    assert_eq!(app(&b).trim(), "hi from util app");
    // Cargo's fingerprints accept what was restored: nothing compiles on the next build.
    let fresh = cargo(&sb, &b, &["build", "-v"]);
    assert!(fresh.status.success());
    assert!(
        !text(&fresh.stderr).contains("Compiling"),
        "{}",
        text(&fresh.stderr)
    );

    // A changed library compiles it and the binary that links it.
    write(
        &b,
        "util/src/lib.rs",
        "pub fn greet() -> &'static str {\n    \"changed\"\n}\n",
    );
    let changed = cargo(&sb, &b, &["build", "-q"]);
    assert!(changed.status.success(), "{}", text(&changed.stderr));
    assert_eq!(app(&b).trim(), "changed app");
    assert_eq!(rustc_entries(&sb, &b), 4);
    assert_eq!(hits(&sb, &b), 2);
}

#[test]
fn setup_cargo_writes_and_undoes_the_wrapper() {
    if asc_binary().is_none() {
        return;
    }
    let sb = Sandbox::new();
    let a = workspace(&sb, "a");
    let said = ok(&sb, &a, &["cache", "setup", "cargo", "--no-incremental"]);
    assert!(said.contains("set build.rustc-wrapper = \"asc\""), "{said}");
    let file = std::fs::read_to_string(a.join(".cargo/config.toml")).unwrap();
    assert!(
        file.contains("rustc-wrapper = \"asc\"") && file.contains("incremental = false"),
        "{file}"
    );
    assert!(
        ok(&sb, &a, &["cache", "setup", "cargo", "--no-incremental"]).contains("nothing to change")
    );
    let undone = ok(&sb, &a, &["cache", "setup", "cargo", "--undo"]);
    assert!(undone.contains("removed build.rustc-wrapper"), "{undone}");
    assert_eq!(
        std::fs::read_to_string(a.join(".cargo/config.toml"))
            .unwrap()
            .trim(),
        ""
    );
}
