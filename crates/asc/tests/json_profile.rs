//! M01c acceptance: `status`, `diff` and `log` print `--json` through the asc JSON profile
//! (SPEC §22.4.2): snake_case names, lowercase hex ids, UTF-8 paths and text, `tz`, numbers for
//! 64-bit integers, and one JSON object per line. `asc api` stays standard proto3 JSON.

mod support;

use serde_json::Value;
use support::{Fixture, text};

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run(cmd: &mut std::process::Command) -> Out {
    let out = cmd.output().expect("spawn");
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: text(&out.stdout),
        stderr: text(&out.stderr),
    }
}

fn lines(out: &Out) -> Vec<Value> {
    assert_eq!(out.code, 0, "{}", out.stderr);
    out.stdout
        .lines()
        .map(|l| serde_json::from_str(l).expect("a JSON line"))
        .collect()
}

fn is_hex(v: &Value, len: usize) -> bool {
    v.as_str().is_some_and(|s| {
        s.len() == len
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// HEAD has `a.txt` and `b.txt`; the work tree edits `a.txt` and adds `u.txt`.
async fn fixture() -> Fixture {
    let f = Fixture::new().await;
    let a = f
        .commit(&[], &[("a.txt", b"one\ntwo\n"), ("b.txt", b"bee\n")])
        .await;
    let b = f.commit(&[a], &[("c.txt", b"sea\n")]).await;
    f.set_branch("master", b).await;
    f.checkout_branch("master", Some(b)).await;
    std::fs::write(f.root.join("a.txt"), "one\nTWO\n").unwrap();
    std::fs::write(f.root.join("b.txt"), "bee\n").unwrap();
    std::fs::write(f.root.join("c.txt"), "sea\n").unwrap();
    std::fs::write(f.root.join("u.txt"), "u\n").unwrap();
    f
}

#[tokio::test]
async fn status_prints_the_profile() {
    let f = fixture().await;
    let out = run(f.asc().args(["status", "--json"]));
    let ls = lines(&out);
    assert_eq!(ls.len(), 1, "{}", out.stdout);
    let v = &ls[0];
    assert!(v.is_object());
    assert_eq!(v["head_ref"], "refs/heads/master");
    assert!(is_hex(&v["head"], 40), "{}", v["head"]);
    let files = v["files"].as_array().unwrap();
    let find = |p: &str| files.iter().find(|f| f["path"] == p).expect(p);
    assert_eq!(find("a.txt")["xy"], ".M");
    assert_eq!(find("u.txt")["xy"], "??");
    assert_eq!(find("a.txt")["mode_worktree"], 0o100644);
    assert!(is_hex(&find("a.txt")["git_head"], 40));
    assert!(v.get("headRef").is_none(), "no camelCase names");
}

#[tokio::test]
async fn diff_prints_the_profile() {
    let f = fixture().await;
    let out = run(f.asc().args(["diff", "--json"]));
    let ls = lines(&out);
    assert_eq!(ls.len(), 1, "{}", out.stdout);
    let ch = &ls[0]["changes"][0];
    assert_eq!(ch["path"], "a.txt");
    assert_eq!(ch["status"], "M");
    assert_eq!(ch["old_mode"], 0o100644);
    assert!(is_hex(&ch["old_git"], 40));
    assert!(is_hex(&ch["old_id"], 64));
    assert_eq!(ch["old_size"], 8);
    let patch = ls[0]["text"].as_str().expect("text is a UTF-8 string");
    assert!(patch.contains("-two\n+TWO\n"), "{patch}");
}

#[tokio::test]
async fn log_prints_the_profile() {
    let f = fixture().await;
    let out = run(f.asc().args(["log", "--json"]));
    let ls = lines(&out);
    assert_eq!(ls.len(), 1, "{}", out.stdout);
    assert_eq!(ls[0]["count"], 2);
    let e = ls[0]["entries"].as_array().unwrap();
    assert_eq!(e.len(), 2);
    assert!(is_hex(&e[0]["id"], 64));
    assert!(is_hex(&e[0]["git"], 40));
    assert_eq!(e[0]["parents"][0], e[1]["id"]);
    assert_eq!(e[0]["author"]["name"], "Test");
    assert_eq!(e[0]["author"]["email"], "test@example.com");
    assert_eq!(e[0]["author"]["time"], 1_700_000_000);
    assert_eq!(e[0]["author"]["tz"], "+0000");
    assert!(e[0]["author"].get("tz_min").is_none());
    assert_eq!(e[0]["message"], "commit of 1 files\n");
}

#[tokio::test]
async fn asc_api_keeps_standard_proto3_json() {
    // `asc api` is outside the profile (SPEC §7): the shared writer is the only place that
    // switched, so its source must not be reachable from the `api` verb.
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/cli/api.rs"));
    if let Ok(src) = src {
        assert!(
            !src.contains("asc_rpc::json"),
            "asc api must stay proto3 JSON"
        );
        assert!(
            !src.contains("JsonLines"),
            "asc api must not use the profile writer"
        );
    }
}
