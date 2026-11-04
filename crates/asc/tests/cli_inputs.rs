//! `asc inputs` through the built binary: clean directories are one tree id, changed and
//! untracked files are hashed from the work tree, ignored and deleted files are left out, and
//! the digest is the same in two repositories at different paths that hold the same content.

mod support;

use std::fs;
use std::path::Path;

use asc_core::id::FileHasher;
use support::{Fixture, text};

const FILES: &[(&str, &[u8])] = &[
    (".gitignore", b"target/\n"),
    ("README.md", b"readme\n"),
    ("app/Cargo.toml", b"[package]\n"),
    ("app/src/main.rs", b"fn main() {}\n"),
    ("app/src/util.rs", b"pub fn f() {}\n"),
    ("lib/x.rs", b"x\n"),
];

fn put(root: &Path, rel: &str, content: &[u8]) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

async fn repo() -> Fixture {
    let f = Fixture::new().await;
    let cs = f.commit(&[], FILES).await;
    f.set_branch("master", cs).await;
    f.checkout_branch("master", Some(cs)).await;
    for (path, content) in FILES {
        put(&f.root, path, content);
    }
    f
}

fn inputs(f: &Fixture, args: &[&str]) -> (i32, String, String) {
    let out = f.asc().arg("inputs").args(args).output().expect("spawn");
    (
        out.status.code().unwrap_or(-1),
        text(&out.stdout),
        text(&out.stderr),
    )
}

fn ok(f: &Fixture, args: &[&str]) -> String {
    let (code, stdout, stderr) = inputs(f, args);
    assert_eq!(code, 0, "{stderr}");
    stdout
}

fn hash(bytes: &[u8]) -> String {
    let mut h = FileHasher::new();
    h.update(bytes);
    h.finalize().to_string()
}

/// `(kind, id, path)` of each line.
fn rows(stdout: &str) -> Vec<(String, String, String)> {
    stdout
        .lines()
        .map(|l| {
            let mut it = l.splitn(3, ' ');
            let k = it.next().unwrap().to_owned();
            let id = it.next().unwrap().to_owned();
            (k, id, it.next().unwrap().to_owned())
        })
        .collect()
}

/// Edits app/src/main.rs, adds app/new.rs, an ignored app/target/out and deletes lib/x.rs.
fn change(f: &Fixture) {
    put(&f.root, "app/src/main.rs", b"fn main() { run() }\n");
    put(&f.root, "app/new.rs", b"new\n");
    put(&f.root, "app/target/out", b"build output\n");
    fs::remove_file(f.root.join("lib/x.rs")).unwrap();
}

#[tokio::test]
async fn clean_directories_are_one_tree() {
    let f = repo().await;
    let all = rows(&ok(&f, &[]));
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!((all[0].0.as_str(), all[0].2.as_str()), ("tree", "."));

    let some = rows(&ok(&f, &["app", "lib", "app/src"]));
    let kinds: Vec<_> = some.iter().map(|r| (r.0.as_str(), r.2.as_str())).collect();
    assert_eq!(
        kinds,
        [("tree", "app"), ("tree", "lib")],
        "app/src is inside app"
    );

    let file = rows(&ok(&f, &["README.md"]));
    assert_eq!(
        file,
        [("file".into(), hash(b"readme\n"), "README.md".into())]
    );
}

#[tokio::test]
async fn changed_files_are_hashed_and_the_rest_come_from_the_tree() {
    let f = repo().await;
    change(&f);
    let app = rows(&ok(&f, &["app"]));
    let want = [
        ("file", hash(b"[package]\n"), "app/Cargo.toml"),
        ("file", hash(b"new\n"), "app/new.rs"),
        ("file", hash(b"fn main() { run() }\n"), "app/src/main.rs"),
        ("file", hash(b"pub fn f() {}\n"), "app/src/util.rs"),
    ];
    let got: Vec<_> = app
        .iter()
        .map(|r| (r.0.as_str(), r.1.clone(), r.2.as_str()))
        .collect();
    assert_eq!(got, want, "ignored app/target is left out");

    // lib/x.rs is gone: lib matches nothing.
    let (code, _, stderr) = inputs(&f, &["lib"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("matches no tracked or untracked file"),
        "{stderr}"
    );
    assert_eq!(ok(&f, &["--allow-missing", "lib"]), "");
}

#[tokio::test]
async fn the_digest_is_the_same_in_every_copy() {
    let a = repo().await;
    let b = repo().await;
    assert_ne!(a.root, b.root);
    let clean = ok(&a, &["--digest"]);
    assert_eq!(clean.trim().len(), 64);
    assert_eq!(clean, ok(&b, &["--digest"]));

    change(&a);
    let changed = ok(&a, &["--digest"]);
    assert_ne!(changed, clean);
    assert_ne!(changed, ok(&b, &["--digest"]));
    change(&b);
    assert_eq!(changed, ok(&b, &["--digest"]));

    // The same paths named from a subdirectory give the same answer.
    let from_sub = f_in(&a, "app", &["--digest", "."]);
    assert_eq!(from_sub, ok(&a, &["--digest", "app"]));

    let json: serde_json::Value = serde_json::from_str(&ok(&a, &["--json", "app"])).unwrap();
    assert_eq!(json["entries"].as_array().unwrap().len(), 4);
    assert_eq!(json["entries"][0]["path"], "app/Cargo.toml");
    assert_eq!(json["digest"].as_str().unwrap().len(), 64);
}

fn f_in(f: &Fixture, sub: &str, args: &[&str]) -> String {
    let out = f
        .asc()
        .current_dir(f.root.join(sub))
        .arg("inputs")
        .args(args)
        .output()
        .expect("spawn");
    assert!(out.status.success(), "{}", text(&out.stderr));
    text(&out.stdout)
}
