//! M02 acceptance: every SPEC 20.7 `--json` shape validates against its `cli.proto` message in
//! the asc JSON profile (`asc_rpc::json`, SPEC 22.4.2), and `asc op log --json` and `asc undo --json`
//! print the shapes of the live binary through the same messages.

mod support;

use std::fs;
use std::sync::OnceLock;

use asc_rpc::json::{decode, encode};
use prost_reflect::{DescriptorPool, MessageDescriptor};
use serde_json::{Value, json};
use support::{Sandbox, text};

fn desc(name: &str) -> MessageDescriptor {
    static POOL: OnceLock<DescriptorPool> = OnceLock::new();
    POOL.get_or_init(|| DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).unwrap())
        .get_message_by_name(&format!("asc.v1.{name}"))
        .unwrap_or_else(|| panic!("asc.v1.{name} is not in the descriptor set"))
}

/// The CLI prints `null`s, empty lists and zero values that the profile codec leaves out.
fn trim(v: &Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), trim(v)))
                .filter(|(_, v)| !is_default(v))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(trim).collect()),
        other => other.clone(),
    }
}

fn is_default(v: &Value) -> bool {
    match v {
        Value::Null | Value::Bool(false) => true,
        Value::Array(a) => a.is_empty(),
        Value::Number(n) => n.as_u64() == Some(0),
        Value::String(s) => s.is_empty(),
        _ => false,
    }
}

/// Decodes `doc` as `message` and checks that encoding it again gives the same document.
fn round_trip(message: &str, doc: &Value) {
    let m = decode(&desc(message), doc).unwrap_or_else(|e| panic!("{message}: {e}\n{doc}"));
    let back = encode(&m).unwrap();
    assert_eq!(back, trim(doc), "{message} does not round-trip");
}

fn change(c: char) -> String {
    c.to_string().repeat(32)
}

fn sha(c: char) -> String {
    c.to_string().repeat(40)
}

#[test]
fn op_result_shape_validates() {
    let doc = json!({
        "version": 1, "op": "0123abcd", "status": "paused",
        "rewritten": [{"change": change('k'), "old": sha('a'), "new": sha('b')}],
        "refs": [
            {"name": "refs/heads/x", "old": sha('a'), "new": sha('b')},
            {"name": "refs/heads/made", "old": null, "new": sha('c')}
        ],
        "skipped": [
            {"change": change('m'), "reason": "checked_out_elsewhere"},
            {"change": change('n'), "reason": "frozen"},
            {"change": change('o'), "reason": "conflict"},
            {"change": change('p'), "reason": "merge"},
            {"change": change('q'), "reason": "immutable"}
        ],
        "conflicts": [{"change": change('r'), "paths": ["src/a.rs", "b c.txt"]}],
        "candidates": [{"change": change('s'), "subject": "fix the parser"}],
        "reviews": [{"change": change('t'), "number": 42, "url": "https://forge.example/pulls/42",
                     "created": true, "version": 4}],
        "hints": ["a hint"]
    });
    round_trip("OpResult", &doc);
    for status in ["ok", "nothing", "paused", "ambiguous"] {
        let d = json!({"version": 1, "op": null, "status": status, "rewritten": [], "refs": [],
            "skipped": [], "conflicts": [], "candidates": [], "reviews": [], "hints": []});
        let m = decode(&desc("OpResult"), &d).unwrap();
        assert_eq!(encode(&m).unwrap()["version"], 1);
    }
}

#[test]
fn op_log_shape_validates_and_is_wrapped() {
    let doc = json!({"version": 1, "ops": [{
        "op": "0123abcd", "seq": 17, "at": 1_700_000_000, "ws": "w1",
        "argv": ["git", "rebase", "master"],
        "refs": [{"name": "refs/heads/master", "old": sha('a'), "new": sha('b')}],
        "snapshot": null, "undone": false
    }, {
        "op": "4567", "seq": 18, "at": 1_700_000_001, "ws": "w1", "argv": ["undo"],
        "refs": [], "snapshot": sha('d'), "undone": true
    }]});
    round_trip("OpLogResponse", &doc);
    // A bare array is not an OpLogResponse.
    assert!(decode(&desc("OpLogResponse"), &json!([])).is_err());
}

#[test]
fn handles_and_plugin_inputs_validate() {
    round_trip(
        "AscHandles",
        &json!({"asc_change": change('k'), "head": sha('a'), "merge_base": sha('b'),
                "stack_parent": change('l'), "version": 3}),
    );
    round_trip(
        "StackTodo",
        &json!({"items": [
            {"action": "pick", "change": "kkkk"},
            {"action": "reword", "change": "llll", "message": "new words"},
            {"action": "fold", "change": "mmmm"},
            {"action": "drop", "change": "nnnn"}
        ]}),
    );
    round_trip(
        "SplitSpec",
        &json!({"pieces": [
            {"message": "one", "patch": "ZGlmZiAtLWdpdA==", "whole_files": ["new.txt"]},
            {"message": "rest"}
        ]}),
    );
    round_trip(
        "WorktreeAddResult",
        &json!({"path": "/tmp/wt", "rev": sha('a'), "ws": "w2", "branch": "agent/x",
                "mode": "plain", "ephemeral": true, "expires_at": 1_700_000_000}),
    );
}

#[test]
fn args_messages_validate() {
    let cases = [
        (
            "CreateArgs",
            json!({"message": "m", "all": true, "onto": "HEAD~1", "insert": true}),
        ),
        (
            "ModifyArgs",
            json!({"message": "m", "all": true, "into": "kkkk", "new_change": true}),
        ),
        ("AbsorbArgs", json!({"apply": true, "into": "kkkk::"})),
        ("RestackArgs", json!({"change": "kkkk", "scope": "upstack"})),
        ("UndoArgs", json!({"op": "0123abcd", "dry_run": true})),
        (
            "SyncArgs",
            json!({"fetch": true, "restack": true, "delete_merged": true}),
        ),
        (
            "SubmitArgs",
            json!({"change": "kkkk", "whole_stack": true, "draft": true,
                   "reviewers": ["ada", "bob"], "update_only": true, "force": true}),
        ),
        (
            "WorktreeAddArgs",
            json!({"path": "wt", "rev": "master", "ephemeral": true, "ttl": "7d", "mode": "plain"}),
        ),
        ("OpLogArgs", json!({"limit": 20})),
    ];
    for (name, doc) in cases {
        round_trip(name, &doc);
    }
    // An explicit `false` of an optional flag survives, so a default of true can be turned off.
    let off = decode(&desc("SyncArgs"), &json!({"restack": false})).unwrap();
    assert_eq!(encode(&off).unwrap(), json!({"restack": false}));
    for scope in ["all", "downstack", "upstack", "only"] {
        decode(&desc("RestackArgs"), &json!({"scope": scope})).unwrap();
    }
}

#[test]
fn typed_details_ride_on_op_result() {
    let details = [
        (
            "restore",
            json!({"paths": ["a.txt"], "staged": true, "worktree": true}),
        ),
        (
            "switch",
            json!({"target": "dev", "head": sha('a'), "created": true}),
        ),
        (
            "cherry_pick",
            json!({"picked": [sha('a')], "made": [sha('b')], "head": sha('b')}),
        ),
        (
            "revert",
            json!({"reverted": [sha('a')], "made": [sha('b')], "head": sha('b')}),
        ),
        (
            "clone",
            json!({"url": "https://forge.example/o/r.git", "path": "/w/r", "mode": "virtual",
                   "branch": "master", "head": sha('a')}),
        ),
        (
            "worktree_add",
            json!({"path": "/w/x", "rev": sha('a'), "ws": "w3", "mode": "plain"}),
        ),
        ("worktree_remove", json!({"path": "/w/x", "forced": true})),
        (
            "worktree_list",
            json!({"worktrees": [{"path": "/w/x", "head": sha('a'), "branch": "m", "locked": true}]}),
        ),
        (
            "stash",
            json!({"action": "list", "entries": [{"index": 0, "commit": sha('a'), "message": "wip"}]}),
        ),
        (
            "fetch",
            json!({"remote": "origin", "updated": [{"name": "refs/remotes/origin/m",
                   "old": sha('a'), "new": sha('b')}], "pruned": ["refs/remotes/origin/gone"]}),
        ),
        (
            "pull",
            json!({"fetch": {"remote": "origin"}, "head": sha('a'), "integrate": "fast-forward"}),
        ),
        (
            "push",
            json!({"remote": "origin", "refs": [{"name": "refs/heads/m", "old": sha('a'),
                   "new": sha('b'), "status": "ok"}]}),
        ),
    ];
    for (name, detail) in details {
        let mut doc = json!({"version": 1, "status": "ok"});
        doc[name] = detail;
        round_trip("OpResult", &doc);
    }
}

#[test]
fn malformed_documents_are_refused() {
    let bad = |name: &str, doc: Value| decode(&desc(name), &doc).is_err();
    assert!(bad(
        "OpResult",
        json!({"version": 1, "status": "ok", "extra": 1})
    ));
    assert!(bad("OpResult", json!({"status": "maybe"})));
    // A change id outside k-z, and a hex id with a bad digit, never become bytes.
    assert!(bad(
        "OpResult",
        json!({"rewritten": [{"change": "abcd", "old": "aa", "new": "bb"}]})
    ));
    assert!(bad(
        "OpResult",
        json!({"rewritten": [{"change": "kkkk", "old": "zz", "new": "bb"}]})
    ));
    assert!(bad("OpResult", json!({"refs": [{"name": 5}]})));
    assert!(bad("UndoArgs", json!({"op": "x", "dry_run": "yes"})));
    assert!(bad("SubmitArgs", json!({"reviewers": "ada"})));
}

struct Repo {
    sb: Sandbox,
    root: std::path::PathBuf,
}

impl Repo {
    fn new() -> Self {
        let sb = Sandbox::new();
        fs::write(
            sb.user.join(".gitconfig"),
            "[user]\n\tname = T\n\temail = t@example.com\n",
        )
        .unwrap();
        let root = sb.dir("repo");
        let r = Self { sb, root };
        r.ok(&["init"]);
        r.commit("a.txt", "one\n", "base");
        r
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self
            .sb
            .asc(&self.root)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("spawn");
        assert_eq!(
            out.status.code(),
            Some(0),
            "asc {args:?}: {}",
            text(&out.stderr)
        );
        text(&out.stdout)
    }

    fn commit(&self, name: &str, body: &str, msg: &str) {
        fs::write(self.root.join(name), body).unwrap();
        self.ok(&["add", name]);
        self.ok(&["commit", "-m", msg]);
    }
}

#[test]
fn live_op_log_json_prints_the_wrapped_shape() {
    let r = Repo::new();
    r.commit("b.txt", "2\n", "two");
    let out = r.ok(&["op", "log", "--json"]);
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert!(v["ops"].is_array(), "{out}");
    assert_eq!(
        v.as_object().unwrap().len(),
        2,
        "only version and ops: {out}"
    );
    round_trip("OpLogResponse", &v);
    assert!(!v["ops"].as_array().unwrap().is_empty());
}

#[test]
fn live_undo_json_prints_an_op_result() {
    let r = Repo::new();
    r.commit("b.txt", "2\n", "two");
    let out = r.ok(&["undo", "--json"]);
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["status"], "ok", "{out}");
    round_trip("OpResult", &v);
}
