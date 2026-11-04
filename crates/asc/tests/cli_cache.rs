//! `asc cache status | ls | show | gc | clear` over a store seeded through `asc-buildcache`, as
//! the adapters write it: outside any repository, in the sandbox's `ASC_HOME`.

mod support;

use asc_buildcache::{Action, ActionKey, Event, EventKind, Origin, Store};
use serde_json::Value;
use support::{Sandbox, asc_binary, text};

fn seed(store: &Store, tool: &str, n: u8, bytes: usize, worktree: &str) -> ActionKey {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("out")).unwrap();
    std::fs::write(dir.path().join("out/a.bin"), vec![n; bytes]).unwrap();
    let origin = Origin {
        worktree: Some(worktree.into()),
        workflow: None,
    };
    let key = ActionKey::builder(tool).part("n", &[n]).finish();
    let mut a = Action::new(tool, &format!("{tool}-{n}"), origin.clone());
    a.outputs = store.put_outputs(dir.path(), &["out".into()]).unwrap();
    store.put_action(&key, &a).unwrap();
    key
}

fn json(sb: &Sandbox, args: &[&str]) -> Value {
    let out = sb.run(&sb.dir("elsewhere"), args);
    assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn status_ls_show_gc_clear() {
    if asc_binary().is_none() {
        return;
    }
    let sb = Sandbox::new();
    let store = Store::open(sb.asc_home.join("buildcache")).unwrap();
    let rustc = seed(&store, "rustc", 1, 3000, "/w/one");
    let vite = seed(&store, "vite", 2, 5000, "/w/one");
    for (k, secs) in [(&rustc, 200), (&vite, 100)] {
        std::fs::File::options()
            .write(true)
            .open(store.entry_path(k))
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(secs))
            .unwrap();
    }
    let used = Origin {
        worktree: Some("/w/two".into()),
        workflow: Some("ci/build".into()),
    };
    assert!(store.lookup("rustc", &rustc, &used).unwrap().is_some());
    store.record(&Event::new(EventKind::Miss, "rustc", &rustc, &used, 0));

    let st = json(&sb, &["cache", "status", "--json"]);
    assert_eq!(st["entries"], 2);
    assert_eq!(st["blobs"], 2);
    assert_eq!(st["max_size"], 20u64 << 30);
    assert_eq!(st["tools"]["rustc"]["week"]["hits"], 1);
    assert_eq!(st["tools"]["rustc"]["week"]["misses"], 1);
    assert_eq!(st["tools"]["rustc"]["day"]["hit_rate"], 0.5);
    let plain = sb.run(&sb.dir("elsewhere"), &["cache"]);
    assert!(plain.status.success(), "{}", text(&plain.stderr));
    assert!(
        text(&plain.stdout).contains("rustc"),
        "{}",
        text(&plain.stdout)
    );

    let ls = json(&sb, &["cache", "ls", "--json"]);
    assert_eq!(ls.as_array().unwrap().len(), 2);
    assert_eq!(
        ls[0]["key"],
        rustc.to_string(),
        "the hit made it most recent"
    );
    let only = json(&sb, &["cache", "ls", "--tool", "vite", "--json"]);
    assert_eq!(only.as_array().unwrap().len(), 1);
    assert_eq!(only[0]["label"], "vite-2");
    assert_eq!(only[0]["size"], 5000);

    let key = rustc.to_string();
    let show = json(&sb, &["cache", "show", &key[..10], "--json"]);
    assert_eq!(show["key"], key);
    assert_eq!(show["outputs"][0]["path"], "out/a.bin");
    assert_eq!(show["hits"], 1);
    assert_eq!(
        show["used_by"]["worktrees"],
        serde_json::json!(["/w/one", "/w/two"])
    );
    assert_eq!(
        show["used_by"]["workflows"],
        serde_json::json!(["ci/build"])
    );
    let human = sb.run(&sb.dir("elsewhere"), &["cache", "show", &key]);
    assert!(
        text(&human.stdout).contains("used by   /w/two"),
        "{}",
        text(&human.stdout)
    );
    let none = sb.run(&sb.dir("elsewhere"), &["cache", "show", "abcdabcd"]);
    assert_eq!(none.status.code(), Some(1));
    assert!(text(&none.stderr).contains("no build cache entry"));

    // 4 KiB keeps the most recent entry (rustc) and evicts vite.
    let gc = json(&sb, &["cache", "gc", "--max-size", "4K", "--json"]);
    assert_eq!(gc["entries"], 1, "{gc}");
    assert_eq!(gc["blobs"], 1, "{gc}");
    assert!(store.get_action(&vite).unwrap().is_none());
    let bad = sb.run(&sb.dir("elsewhere"), &["cache", "gc", "--max-size", "lots"]);
    assert!(!bad.status.success());

    let cl = json(&sb, &["cache", "clear", "--tool", "rustc", "--json"]);
    assert_eq!(cl["entries"], 1);
    assert_eq!(cl["after"]["bytes"], 0);
}

#[test]
fn the_store_follows_config() {
    if asc_binary().is_none() {
        return;
    }
    let sb = Sandbox::new();
    let dir = sb.dir("shared-cache");
    std::fs::write(
        sb.user.join(".gitconfig"),
        format!(
            "[asc \"buildcache\"]\n\tdir = {}\n\tmaxSize = 1G\n",
            dir.to_string_lossy().replace('\\', "/")
        ),
    )
    .unwrap();
    let st = json(&sb, &["cache", "status", "--json"]);
    assert_eq!(st["max_size"], 1u64 << 30);
    let root = st["root"].as_str().unwrap().replace('\\', "/");
    assert_eq!(root, dir.to_string_lossy().replace('\\', "/"));
    assert!(dir.join("cas").is_dir());
}
