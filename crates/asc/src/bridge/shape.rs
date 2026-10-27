//! The `--json` shapes of the allowlisted verbs and how `Bridge.Run` turns them into an `OpResult`.
//!
//! `Bridge.Run` returns one `asc.v1.OpResult` per call (SPEC §24.2, IP-D3). The verbs of the
//! plugin unit (`restore`, `switch`, `fetch`, ...) and the navigation and undo verbs print an
//! `OpResult` themselves. The older stack verbs print reports of their own (`sync`'s report,
//! `restack`'s `rewrites`, the `verb`/`changes` report of `squash` and its kin, ...) that
//! other consumers read, so they keep them. [`adapt`] maps each such report to an `OpResult`
//! value in the asc JSON profile: same status and exit code, ids that the profile accepts as
//! typed fields, and everything else as `hints` text. A value that is already an `OpResult`
//! is not touched (the caller tries the decode first), and an unknown verb or shape is not
//! guessed at: the call ends `INTERNAL`.
//!
//! Two read-only verbs print no `OpResult` at all: `sl` prints a `Stacks` snapshot and `op log`
//! an `OpLogResponse`. `RunEvent` carries only an `OpResult`, so [`reads_elsewhere`] names the
//! reason and the bridge ends the call with `FAILED_PRECONDITION` instead of an undecodable
//! `INTERNAL`.
//! Known gap: `op log` needs a typed `RunEvent` field (or its own method) before a plugin can read it.

use serde_json::{Value, json};

/// Most `hints` lines one result carries.
const MAX_HINTS: usize = 1000;
/// Longest `hints` line.
const MAX_HINT: usize = 2048;

/// Why `verb` has no `OpResult`, when it has none.
pub fn reads_elsewhere(verb: &str) -> Option<&'static str> {
    match verb {
        "sl" => Some(
            "sl prints a Stacks snapshot, not an OpResult: read it with asc.v1.Workspace/Stacks",
        ),
        "op log" => Some("op log prints an OpLogResponse, which Bridge.Run cannot carry"),
        _ => None,
    }
}

/// The `OpResult` form of `report`, the JSON `verb` printed, or `None` for a shape not known.
pub fn adapt(verb: &str, report: &Value) -> Option<Value> {
    if !report.is_object() {
        return None;
    }
    let out = match verb {
        "sync" => sync(report),
        "restack" => restack(report),
        "absorb" => absorb(report),
        "squash" | "fold" | "abandon" | "uncommit" | "reorder" | "hide" | "unhide" | "move"
        | "split" | "stack delete" => rewrite(report)?,
        "stack freeze" | "stack unfreeze" => freeze(report)?,
        "submit" => submit(report),
        "stack merge" => merge(report)?,
        "stack get" => get(report),
        "share" => share(report)?,
        _ => return None,
    };
    Some(out.into_value())
}

#[derive(Default)]
struct Out {
    status: &'static str,
    rewritten: Vec<Value>,
    refs: Vec<Value>,
    conflicts: Vec<Value>,
    candidates: Vec<Value>,
    reviews: Vec<Value>,
    hints: Vec<String>,
}

impl Out {
    fn new(status: &'static str) -> Self {
        Self {
            status,
            ..Self::default()
        }
    }

    fn hint(&mut self, text: impl Into<String>) {
        if self.hints.len() < MAX_HINTS {
            let mut t: String = text.into();
            if t.len() > MAX_HINT {
                let mut end = MAX_HINT;
                while !t.is_char_boundary(end) {
                    end -= 1;
                }
                t.truncate(end);
            }
            self.hints.push(t);
        }
    }

    fn rewritten(&mut self, rows: &[Value]) {
        for r in rows {
            if let (Some(c), Some(o), Some(n)) = (change(r, "change"), hex(r, "old"), hex(r, "new"))
            {
                self.rewritten
                    .push(json!({ "change": c, "old": o, "new": n }));
            }
        }
    }

    fn into_value(self) -> Value {
        json!({
            "version": 1,
            "status": self.status,
            "rewritten": self.rewritten,
            "refs": self.refs,
            "skipped": [],
            "conflicts": self.conflicts,
            "candidates": self.candidates,
            "reviews": self.reviews,
            "hints": self.hints,
        })
    }
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn list<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
}

fn flag(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// A change id in the profile: an even number of letters `k` to `z`.
fn is_change(s: &str) -> bool {
    !s.is_empty() && s.len().is_multiple_of(2) && s.bytes().all(|c| (b'k'..=b'z').contains(&c))
}

/// An id in the profile: an even number of lowercase hex digits.
fn is_hex(s: &str) -> bool {
    !s.is_empty()
        && s.len().is_multiple_of(2)
        && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

fn change<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    Some(text(v, key)).filter(|s| is_change(s))
}

fn hex<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    Some(text(v, key)).filter(|s| is_hex(s))
}

fn short(s: &str) -> String {
    s.chars().take(12).collect()
}

/// `complete`, `applied`, `submitted` and `ok` are done; `paused` is a conflict; the rest is
/// nothing to do. An unknown word is `None`.
fn status_of(word: &str) -> Option<&'static str> {
    Some(match word {
        "complete" | "applied" | "submitted" | "ok" => "ok",
        "nothing" | "preview" | "dry_run" => "nothing",
        "paused" => "paused",
        "ambiguous" => "ambiguous",
        _ => return None,
    })
}

fn status(v: &Value) -> &'static str {
    status_of(text(v, "status")).unwrap_or("ok")
}

// ---------------------------------------------------------------------------- the reports

fn sync(v: &Value) -> Out {
    let moved = !list(v, "fast_forwarded").is_empty()
        || !list(v, "landed").is_empty()
        || !list(v, "restacked").is_empty()
        || !list(v, "deleted_branches").is_empty();
    let dry = flag(v, "dry_run");
    let mut o = Out::new(if moved && !dry { "ok" } else { "nothing" });
    if dry {
        o.hint("dry run: nothing was changed");
    }
    if let Some(r) = v.get("fetched").and_then(Value::as_str) {
        o.hint(format!("fetched {r}"));
    }
    for m in list(v, "fast_forwarded") {
        let name = format!("refs/heads/{}", text(m, "trunk"));
        let mut r = json!({ "name": name });
        if let Some(old) = hex(m, "old") {
            r["old"] = json!(old);
        }
        if let Some(new) = hex(m, "new") {
            r["new"] = json!(new);
        }
        o.refs.push(r);
        o.hint(format!(
            "fast-forwarded {} to {}",
            text(m, "trunk"),
            text(m, "to")
        ));
    }
    for l in list(v, "landed") {
        o.hint(format!(
            "landed {} {} ({})",
            short(text(l, "change")),
            text(l, "subject"),
            text(l, "by")
        ));
    }
    o.rewritten(list(v, "restacked"));
    for r in list(v, "restacked") {
        o.hint(format!(
            "restacked {} {}",
            short(text(r, "change")),
            text(r, "subject")
        ));
    }
    for s in list(v, "skipped") {
        o.hint(format!(
            "skipped {}: {}",
            text(s, "what"),
            text(s, "reason")
        ));
    }
    for c in list(v, "cannot_tell").iter().filter_map(Value::as_str) {
        o.hint(format!("cannot tell whether {c} was merged"));
    }
    for b in list(v, "merged_branches").iter().filter_map(Value::as_str) {
        o.hint(format!("merged branch {b}"));
    }
    for b in list(v, "deleted_branches").iter().filter_map(Value::as_str) {
        o.hint(format!("deleted branch {b}"));
    }
    o
}

fn restack(v: &Value) -> Out {
    let mut o = Out::new(status(v));
    o.rewritten(list(v, "rewrites"));
    if let Some(at) = v.get("paused_at").filter(|a| a.is_object()) {
        match change(at, "change") {
            Some(c) => o.conflicts.push(json!({ "change": c, "paths": [] })),
            None => o.hint(format!("paused at {}", short(text(at, "change")))),
        }
    }
    o
}

fn absorb(v: &Value) -> Out {
    let mut o = Out::new(status(v));
    if text(v, "status") == "preview" {
        o.hint("preview: nothing was changed");
    }
    o.rewritten(list(v, "rewrites"));
    for s in list(v, "skipped") {
        o.hint(format!("kept {}: {}", text(s, "path"), text(s, "reason")));
    }
    o
}

fn rewrite(v: &Value) -> Option<Out> {
    let mut o = Out::new(status_of(text(v, "status"))?);
    let verb = text(v, "verb");
    for c in list(v, "changes") {
        o.hint(format!(
            "{verb} {} {}",
            short(text(c, "change")),
            text(c, "subject")
        ));
    }
    let restacked = v.get("restacked").and_then(Value::as_u64).unwrap_or(0);
    if restacked > 0 {
        o.hint(format!("restacked {restacked}"));
    }
    Some(o)
}

fn freeze(v: &Value) -> Option<Out> {
    let c = text(v, "change");
    let word = if flag(v, "frozen") {
        "frozen"
    } else {
        "unfrozen"
    };
    v.get("changed")?;
    let mut o = Out::new(if flag(v, "changed") { "ok" } else { "nothing" });
    o.hint(format!("{word} {}", short(c)));
    Some(o)
}

fn submit(v: &Value) -> Out {
    let mut o = Out::new(status(v));
    for c in list(v, "changes") {
        let (Some(id), Some(r)) = (
            change(c, "change"),
            c.get("review").filter(|r| r.is_object()),
        ) else {
            continue;
        };
        o.reviews.push(json!({
            "change": id,
            "number": r.get("number").and_then(Value::as_u64).unwrap_or(0),
            "url": text(r, "url"),
            "created": false,
            "version": 0,
        }));
    }
    for c in list(v, "candidates").iter().filter_map(Value::as_str) {
        if is_change(c) {
            o.candidates.push(json!({ "change": c, "subject": "" }));
        } else {
            o.hint(format!("candidate {}", short(c)));
        }
    }
    if text(v, "status") == "dry_run" {
        o.hint("dry run: nothing was submitted");
    }
    for m in list(v, "message").iter().filter_map(Value::as_str) {
        o.hint(m);
    }
    o
}

fn merge(v: &Value) -> Option<Out> {
    let state = text(v, "state");
    let mut o = Out::new(match state {
        "queued" | "landed" => "ok",
        "refused" => "nothing",
        _ => return None,
    });
    o.hint(format!("{} {state}", short(text(v, "change"))));
    for p in list(v, "pending").iter().filter_map(Value::as_str) {
        o.hint(format!("pending {p}"));
    }
    let reason = text(v, "reason");
    if !reason.is_empty() {
        o.hint(reason);
    }
    Some(o)
}

fn get(v: &Value) -> Out {
    let mut o = Out::new("ok");
    o.hint(format!("checked out {}", text(v, "checked_out")));
    if let Some(w) = v.get("worktree").and_then(Value::as_str) {
        o.hint(format!("worktree {w}"));
    }
    for c in list(v, "changes") {
        o.hint(format!("change {}", short(text(c, "change"))));
    }
    o
}

fn share(v: &Value) -> Option<Out> {
    let link = text(v, "link");
    if link.is_empty() {
        return None;
    }
    let mut o = Out::new("ok");
    o.hint(format!("link {link}"));
    Some(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHANGE: &str = "lyoskttzvzkpvpurqtmxuonwxpvmlvtt";
    const ID: &str = "5e6585043a10585a79a29d2e796b509f57b6ac2d";

    fn decodes(v: &Value) -> bool {
        let pool = prost_reflect::DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).unwrap();
        let desc = pool.get_message_by_name("asc.v1.OpResult").unwrap();
        asc_rpc::json::decode(&desc, v).is_ok()
    }

    #[test]
    fn every_adapted_shape_decodes_as_an_op_result() {
        let row = json!({ "change": CHANGE, "subject": "s", "old": ID, "new": ID });
        let cases: Vec<(&str, Value)> = vec![
            (
                "sync",
                json!({
                    "version": 1, "dry_run": false, "fetched": "origin",
                    "fast_forwarded": [{ "trunk": "master", "to": "5e6585043a10", "old": ID, "new": ID }],
                    "landed": [{ "change": CHANGE, "commit": ID, "subject": "s", "by": "patch-id" }],
                    "restacked": [row.clone()],
                    "skipped": [{ "what": "trunk x", "reason": "diverged" }],
                    "cannot_tell": ["b"], "merged_branches": ["m"], "deleted_branches": ["d"],
                }),
            ),
            (
                "sync",
                json!({ "version": 1, "dry_run": false, "fetched": null, "fast_forwarded": [],
                "landed": [], "restacked": [], "skipped": [], "cannot_tell": [],
                "merged_branches": [], "deleted_branches": [] }),
            ),
            (
                "restack",
                json!({ "version": 1, "status": "complete", "rewrites": [row.clone()], "paused_at": null }),
            ),
            (
                "restack",
                json!({ "version": 1, "status": "paused", "rewrites": [], "paused_at": row.clone() }),
            ),
            (
                "absorb",
                json!({ "version": 1, "status": "applied", "hunks": [],
                "skipped": [{ "path": "a", "reason": "r" }], "rewrites": [row.clone()] }),
            ),
            (
                "squash",
                json!({ "version": 1, "verb": "squash", "status": "complete",
                "changes": [{ "change": CHANGE, "subject": "s" }], "restacked": 2 }),
            ),
            (
                "stack delete",
                json!({ "version": 1, "verb": "abandon", "status": "paused", "changes": [], "restacked": 0 }),
            ),
            (
                "stack freeze",
                json!({ "version": 1, "change": CHANGE, "frozen": true, "changed": true }),
            ),
            (
                "submit",
                json!({ "version": 1, "status": "submitted", "remote": "origin", "trunk": "master",
                "draft": true, "reviewers": [], "message": ["m"], "candidates": [],
                "changes": [{ "change": CHANGE, "short": "s", "commit": ID, "asc": "c", "subject": "s",
                    "ref": "r", "review": { "number": 3, "url": "u", "state": "open", "draft": true } }] }),
            ),
            (
                "submit",
                json!({ "version": 1, "status": "ambiguous", "remote": "origin", "trunk": null,
                "draft": true, "reviewers": [], "message": [], "candidates": [CHANGE], "changes": [] }),
            ),
            (
                "stack merge",
                json!({ "version": 1, "change": CHANGE, "state": "queued", "pending": ["p"], "reason": "" }),
            ),
            (
                "stack get",
                json!({ "version": 1, "changes": [{ "change": CHANGE, "commit": ID, "frozen": false }],
                "checked_out": CHANGE, "worktree": "w" }),
            ),
            (
                "share",
                json!({ "version": 1, "id": "i", "link": "l", "expires_at": 5 }),
            ),
        ];
        for (verb, report) in cases {
            let out = adapt(verb, &report).unwrap_or_else(|| panic!("{verb} {report}"));
            assert!(decodes(&out), "{verb}: {out}");
        }
    }

    #[test]
    fn statuses_keep_the_exit_code_meaning() {
        let s = |verb, r: Value| {
            adapt(verb, &r).unwrap()["status"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(
            s("restack", json!({ "status": "paused", "rewrites": [] })),
            "paused"
        );
        assert_eq!(
            s("restack", json!({ "status": "nothing", "rewrites": [] })),
            "nothing"
        );
        assert_eq!(
            s("sync", json!({ "dry_run": true, "landed": [{}] })),
            "nothing"
        );
        assert_eq!(s("sync", json!({ "landed": [{}] })), "ok");
        assert_eq!(s("submit", json!({ "status": "ambiguous" })), "ambiguous");
    }

    #[test]
    fn an_id_the_profile_would_refuse_is_left_out() {
        let r = json!({ "status": "complete", "rewrites": [{ "change": "xx!", "old": "zz", "new": ID }] });
        let out = adapt("restack", &r).unwrap();
        assert!(out["rewritten"].as_array().unwrap().is_empty());
        assert!(decodes(&out));
    }

    #[test]
    fn unknown_verbs_and_shapes_are_not_guessed() {
        assert!(adapt("create", &json!({ "version": 1 })).is_none());
        assert!(adapt("sync", &json!([1])).is_none());
        assert!(adapt("squash", &json!({ "status": "weird" })).is_none());
        assert!(reads_elsewhere("sl").is_some() && reads_elsewhere("op log").is_some());
        assert!(reads_elsewhere("sync").is_none());
    }
}
