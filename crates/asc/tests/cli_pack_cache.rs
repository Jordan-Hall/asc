//! Whole-pack caching for bucket remotes (docs/design/PACKED-OBJECTS.md, "Tiered store and
//! cache"): a clone from a bucket whose objects are packed keeps each small pack whole in the
//! object cache (clones use `--refs shared`: each ASC_HOME is its own machine), and `asc.cache.packFetchMax 0` reads by range and keeps nothing.

mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use asc_store::bucket::BucketObjects;
use asc_store::gc::NoFence;
use asc_store::pack::PackConfig;
use support::{Sandbox, text};

const IDENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

fn run(sb: &Sandbox, home: &Path, cwd: &Path, args: &[&str]) -> String {
    let out = sb
        .asc(cwd)
        .env("ASC_HOME", home)
        .envs(IDENT)
        .args(args)
        .output()
        .expect("spawn asc");
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

/// The packs below `<home>/cache/pack/<scope>/`.
fn cached_packs(home: &Path) -> Vec<PathBuf> {
    let Ok(scopes) = std::fs::read_dir(home.join("cache").join("pack")) else {
        return Vec::new();
    };
    scopes
        .flatten()
        .flat_map(|s| std::fs::read_dir(s.path()).into_iter().flatten().flatten())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "pack"))
        .collect()
}

/// A bucket holding one pushed commit of three files, its objects packed.
async fn packed_bucket(sb: &Sandbox) -> String {
    let bucket = sb.dir("bucket");
    let url = bucket_url(&bucket);
    let src = sb.dir("src");
    let home = sb.asc_home.clone();
    run(
        sb,
        &home,
        &src,
        &["init", "-q", "--refs", "shared", "--store", &url],
    );
    for (name, body) in [
        ("a.txt", "alpha\n"),
        ("b.txt", "beta\n"),
        ("c.txt", "gamma\n"),
    ] {
        std::fs::write(src.join(name), body).unwrap();
        run(sb, &home, &src, &["add", name]);
    }
    run(sb, &home, &src, &["commit", "-q", "-m", "three"]);
    run(sb, &home, &src, &["push", "-q", "-u", "origin", "master"]);

    let objects = BucketObjects::local(&bucket)
        .unwrap()
        .with_bucket_packs(PackConfig {
            enabled: true,
            min_age: Duration::ZERO,
            min_loose: 1,
            negative_ttl: Duration::ZERO,
            ..PackConfig::default()
        });
    let later = SystemTime::now() + Duration::from_secs(3600);
    let rep = objects.pack_run(&NoFence, later, true).await.unwrap();
    assert!(rep.packed > 0, "{rep:?}");
    url
}

#[tokio::test]
async fn a_clone_keeps_small_packs_whole_in_the_cache() {
    let sb = Sandbox::new();
    let url = packed_bucket(&sb).await;

    let home = sb.dir("home2");
    let cwd = sb.dir("cwd");
    run(
        &sb,
        &home,
        &cwd,
        &["clone", "-q", "--refs", "shared", &url, "c"],
    );
    let c = cwd.join("c");
    assert_eq!(run(&sb, &home, &c, &["show", "HEAD:b.txt"]), "beta\n");
    assert!(!cached_packs(&home).is_empty(), "the pack is cached whole");

    // A limit of 0 reads by range: the clone works and nothing is kept.
    let home3 = sb.dir("home3");
    let cwd3 = sb.dir("cwd3");
    run(
        &sb,
        &home3,
        &cwd3,
        &["config", "--global", "asc.cache.packFetchMax", "0"],
    );
    run(
        &sb,
        &home3,
        &cwd3,
        &["clone", "-q", "--refs", "shared", &url, "c"],
    );
    assert_eq!(
        run(&sb, &home3, &cwd3.join("c"), &["show", "HEAD:c.txt"]),
        "gamma\n"
    );
    assert!(cached_packs(&home3).is_empty());

    // A bad limit is refused the way git refuses a bad number.
    let out = sb
        .asc(&c)
        .env("ASC_HOME", &home)
        .args(["-c", "asc.cache.packFetchMax=lots", "fetch", "-q"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("asc.cache.packFetchMax"));
}
