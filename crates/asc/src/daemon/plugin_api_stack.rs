//! Workspace plugin additions of SPEC §20.6 (C31): `Stacks`, `Worktrees`, `Conflicts`,
//! `CommitGraph` (with a WIP row per worktree) and the `buffer` / `ignore_whitespace` fields of
//! `Blame`. The contract and the bounds are in `docs/plugin-api-stack.md`.
//!
//! Like the RPCs of [`super::plugin_api`], each query runs the matching CLI command in the mounted
//! workspace and decodes its JSON lines, so an RPC answers exactly what the CLI prints:
//! `Stacks` is `asc sl --json`; the others are the `asc ws-api` plumbing verb. Authorisation, the
//! root re-check, the output caps and the run limit are those of `plugin_api`.
//!
//! `Blame` with a `buffer` blames the file at the revision (default `HEAD`), reads the revision's
//! blob, maps each buffer line to a blob line by one line diff (bounded in time), and labels a line
//! that has no counterpart as uncommitted (an empty `origin`).

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use asc_rpc::server::wire;
use serde_json::Value;
use tonic::Status;

use super::plugin_api::{
    MAX_OUT, PluginStream, bad, conv_blame, decode_line, first_line, internal, mounted, os,
    path_args, run_collect, stream_asc, text_arg,
};
use super::service::State;

/// Most revisions in `include` plus `exclude`.
const MAX_REVS: usize = 64;
/// Longest page token (hex digits).
const MAX_TOKEN: usize = 8192;
/// Largest `buffer` (a request message is at most 4 MiB on the wire; this bounds the diff too).
const MAX_BUFFER: usize = 8 * 1024 * 1024;
/// Most lines of a buffer or a blob that the line diff maps.
const MAX_LINES: usize = 500_000;
/// Hunks per `BlameResponse` message of a buffer blame.
const HUNKS_PER_MESSAGE: usize = 500;
/// Most worktrees, stacks or changes the converters accept from the child (it is capped already).
const MAX_ITEMS: usize = 100_000;

// ------------------------------------------------------------------------------ Stacks

fn field<'a>(v: &'a Value, name: &str) -> Result<&'a Value, Status> {
    v.get(name)
        .ok_or_else(|| internal(format!("asc sl printed no '{name}'")))
}

fn text(v: &Value, name: &str) -> Result<String, Status> {
    match field(v, name)? {
        Value::String(s) => Ok(s.clone()),
        Value::Null => Ok(String::new()),
        _ => Err(internal(format!("asc sl printed a bad '{name}'"))),
    }
}

fn flag(v: &Value, name: &str) -> bool {
    v.get(name).and_then(Value::as_bool).unwrap_or(false)
}

fn number(v: &Value, name: &str) -> u64 {
    v.get(name).and_then(Value::as_u64).unwrap_or(0)
}

/// A change id as its 16 bytes; text that is not a change id is its own bytes.
fn change_bytes(t: &str) -> Bytes {
    match t.parse::<asc_core::change::ChangeId>() {
        Ok(id) => Bytes::copy_from_slice(id.as_bytes()),
        Err(_) => Bytes::copy_from_slice(t.as_bytes()),
    }
}

/// Lowercase hex as bytes; any other text (a change id that is not hex) as its own bytes.
fn id_bytes(t: &str) -> Bytes {
    let hex =
        !t.is_empty() && t.len().is_multiple_of(2) && t.bytes().all(|b| b.is_ascii_hexdigit());
    if hex {
        let raw: Option<Vec<u8>> = (0..t.len() / 2)
            .map(|i| u8::from_str_radix(&t[2 * i..2 * i + 2], 16).ok())
            .collect();
        if let Some(raw) = raw {
            return raw.into();
        }
    }
    Bytes::copy_from_slice(t.as_bytes())
}

/// Minutes east of UTC from git's `+0530` form; 0 for anything else.
fn tz_minutes(t: &str) -> i32 {
    let b = t.as_bytes();
    if b.len() != 5 || !matches!(b[0], b'+' | b'-') || !b[1..].iter().all(u8::is_ascii_digit) {
        return 0;
    }
    let n = |i: usize| i32::from(b[i] - b'0') * 10 + i32::from(b[i + 1] - b'0');
    let m = n(1) * 60 + n(3);
    if b[0] == b'-' { -m } else { m }
}

fn review_of(v: &Value) -> wire::ReviewState {
    let state = v.get("state").and_then(Value::as_str).unwrap_or_default();
    let word = |name: &str| {
        v.get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    wire::ReviewState {
        number: number(v, "number"),
        url: word("url"),
        state: state.to_owned(),
        draft: state == "draft",
        approvals: u32::try_from(number(v, "approvals")).unwrap_or(u32::MAX),
        checks: word("checks"),
        queue: word("queue"),
        reviewed_version: u32::try_from(number(v, "reviewed_version")).unwrap_or(u32::MAX),
        ..wire::ReviewState::default()
    }
}

fn entry_of(c: &Value) -> Result<wire::StackEntry, Status> {
    let a = field(c, "author")?;
    let author = wire::Signature {
        name: text(a, "name")?.into_bytes().into(),
        email: text(a, "email")?.into_bytes().into(),
        time: a.get("time").and_then(Value::as_i64).unwrap_or(0),
        tz_min: tz_minutes(&text(a, "tz")?),
    };
    let branches = match c.get("branches") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|b| b.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    };
    Ok(wire::StackEntry {
        change: change_bytes(&text(c, "change")?),
        commit: id_bytes(&text(c, "commit")?),
        asc: id_bytes(&text(c, "asc")?),
        parent_change: change_bytes(&text(c, "parent_change")?),
        branches,
        subject: text(c, "subject")?.into_bytes().into(),
        author: Some(author),
        state: text(c, "state")?,
        needs_restack: flag(c, "needs_restack"),
        frozen: flag(c, "frozen"),
        empty: flag(c, "empty"),
        current: flag(c, "current"),
        worktree: text(c, "worktree")?,
        review: c.get("review").filter(|r| r.is_object()).map(review_of),
        short: text(c, "short")?,
    })
}

/// The `Stacks` answer for the document `asc sl --json` prints.
pub(super) fn stacks_of(doc: &Value) -> Result<wire::WsStacksResponse, Status> {
    let list = |name: &str| -> Result<&Vec<Value>, Status> {
        field(doc, name)?
            .as_array()
            .filter(|a| a.len() <= MAX_ITEMS)
            .ok_or_else(|| internal(format!("asc sl printed a bad '{name}'")))
    };
    let mut trunks = Vec::new();
    for t in list("trunks")? {
        trunks.push(wire::StackTrunk {
            name: text(t, "name")?,
            tip: id_bytes(&text(t, "tip")?),
            remote: text(t, "remote")?,
        });
    }
    let mut stacks = Vec::new();
    for s in list("stacks")? {
        let mut changes = Vec::new();
        for c in field(s, "changes")?
            .as_array()
            .ok_or_else(|| internal("asc sl printed bad 'changes'"))?
        {
            changes.push(entry_of(c)?);
        }
        stacks.push(wire::WsStack {
            trunk: text(s, "trunk")?,
            base: id_bytes(&text(s, "base")?),
            behind: u32::try_from(number(s, "behind")).unwrap_or(u32::MAX),
            changes,
        });
    }
    Ok(wire::WsStacksResponse {
        version: u32::try_from(number(doc, "version")).unwrap_or(1),
        trunks,
        stacks,
    })
}

pub async fn stacks(
    state: &Arc<State>,
    r: wire::WsStacksRequest,
) -> Result<wire::WsStacksResponse, Status> {
    let m = mounted(state, &r.ws_id)?;
    let mut args = os(&["sl", "--json"]);
    if r.all {
        args.push("-a".into());
    }
    if r.remote {
        args.push("--remote".into());
    }
    if !r.revset.is_empty() {
        args.push(format!("--revset={}", text_arg(&r.revset, "the revset", false)?).into());
    }
    let out = run_collect(&m, args, None).await?;
    let doc: Value = serde_json::from_slice(first_line(&out)?)
        .map_err(|e| internal(format!("asc sl printed bad JSON: {e}")))?;
    stacks_of(&doc)
}

// ------------------------------------------------------------------------------ Worktrees, Conflicts

/// The worktrees of every repository that has a mounted workspace: one `asc ws-api worktrees` per
/// repository, run in a mounted workspace of it. A repository with no mounted workspace is not
/// listed (the daemon touches only workspaces it hosts).
pub async fn worktrees(
    state: &Arc<State>,
    _r: wire::WsWorktreesRequest,
) -> Result<wire::WsWorktreesResponse, Status> {
    let mut seen: Vec<i64> = Vec::new();
    let mut all = Vec::new();
    for m in state.mounts.all() {
        let repo = m.row.ws.repo.0;
        if seen.contains(&repo) {
            continue;
        }
        seen.push(repo);
        let out = run_collect(&m, os(&["ws-api", "worktrees"]), None).await?;
        let one: wire::WsWorktreesResponse =
            decode_line("asc.v1.WsWorktreesResponse", first_line(&out)?)?
                .transcode_to()
                .map_err(internal)?;
        all.extend(one.worktrees);
        if all.len() > MAX_ITEMS {
            return Err(Status::resource_exhausted("too many worktrees"));
        }
    }
    Ok(wire::WsWorktreesResponse { worktrees: all })
}

pub async fn conflicts(
    state: &Arc<State>,
    r: wire::WsConflictsRequest,
) -> Result<wire::WsConflictsResponse, Status> {
    let m = mounted(state, &r.ws_id)?;
    let out = run_collect(&m, os(&["ws-api", "conflicts"]), None).await?;
    decode_line("asc.v1.WsConflictsResponse", first_line(&out)?)?
        .transcode_to()
        .map_err(internal)
}

// ------------------------------------------------------------------------------ CommitGraph

fn conv_graph(d: prost_reflect::DynamicMessage) -> Result<wire::CommitGraphResponse, Status> {
    d.transcode_to().map_err(internal)
}

pub async fn commit_graph(
    state: &Arc<State>,
    r: wire::WsCommitGraphRequest,
) -> Result<PluginStream<wire::CommitGraphResponse>, Status> {
    let m = mounted(state, &r.ws_id)?;
    let q = r.query.unwrap_or_default();
    if q.include.len() > MAX_REVS || q.exclude.len() > MAX_REVS {
        return Err(bad(format!("more than {MAX_REVS} revisions")));
    }
    if wire::GraphPreset::try_from(q.preset).is_err() {
        return Err(bad("unknown graph preset"));
    }
    if q.page_token.len() > MAX_TOKEN || !q.page_token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad("malformed page token"));
    }
    let mut args = os(&["ws-api", "graph"]);
    for s in &q.include {
        args.push(format!("--include={}", text_arg(s, "a revision", true)?).into());
    }
    for s in &q.exclude {
        args.push(format!("--exclude={}", text_arg(s, "a revision", true)?).into());
    }
    args.push(format!("--preset={}", q.preset).into());
    args.push(format!("--limit={}", q.limit).into());
    if q.lanes {
        args.push("--lanes".into());
    }
    if q.stats {
        args.push("--stats".into());
    }
    if r.wip_rows {
        args.push("--wip".into());
    }
    if !q.page_token.is_empty() {
        args.push(format!("--page-token={}", q.page_token).into());
    }
    stream_asc(&m, args, "asc.v1.CommitGraphResponse", conv_graph)
}

// ------------------------------------------------------------------------------ Blame with a buffer

/// Where one line came from.
#[derive(Clone, PartialEq, Eq)]
struct Origin {
    id: Bytes,
    path: Bytes,
    line: u32,
}

/// `line` split into lines, each keeping its terminator.
fn lines_of(data: &[u8]) -> Vec<&[u8]> {
    data.split_inclusive(|b| *b == b'\n').collect()
}

/// The origin of every line of the blob, from the hunks of a blame of that blob.
fn origins_of(hunks: &[wire::BlameHunk], lines: usize) -> Vec<Option<Origin>> {
    let mut out: Vec<Option<Origin>> = vec![None; lines];
    for h in hunks {
        for k in 0..h.len {
            let at = (h.start + k).saturating_sub(1) as usize;
            if let Some(slot) = out.get_mut(at) {
                *slot = Some(Origin {
                    id: h.origin.clone(),
                    path: h.orig_path.clone(),
                    line: h.orig_start + k,
                });
            }
        }
    }
    out
}

/// For each buffer line, the blob line it equals (by one line diff), if any.
fn map_lines(old: &[u8], new: &[u8], ignore_whitespace: bool) -> Vec<Option<usize>> {
    use asc_engine::diff::line::{Algorithm, diff};
    let norm = |data: &[u8]| -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        for l in lines_of(data) {
            out.extend(l.iter().copied().filter(|b| !b.is_ascii_whitespace()));
            out.push(b'\n');
        }
        out
    };
    let (o, n);
    let d = if ignore_whitespace {
        (o, n) = (norm(old), norm(new));
        diff(&o, &n, Algorithm::Myers, false)
    } else {
        diff(old, new, Algorithm::Myers, false)
    };
    // Between the changed ranges, old and new lines pair up one to one.
    let mut map = vec![None; d.new.len()];
    let (mut i, mut j) = (0usize, 0usize);
    for h in &d.hunks {
        for k in 0..(h.after.start as usize).saturating_sub(j) {
            map[j + k] = Some(i + k);
        }
        i = h.before.end as usize;
        j = h.after.end as usize;
    }
    for k in 0..d.new.len().saturating_sub(j) {
        map[j + k] = Some(i + k);
    }
    map
}

/// The hunks of the buffer's lines `first..=last` (one-based), merged where consecutive lines
/// continue one origin; a line with no origin is uncommitted.
fn hunks_of(
    origins: &[Option<Origin>],
    path: &Bytes,
    first: usize,
    last: usize,
) -> Vec<wire::BlameHunk> {
    let mut out: Vec<wire::BlameHunk> = Vec::new();
    for n in first..=last {
        let o = origins.get(n - 1).cloned().flatten();
        let (id, op, ol) = match &o {
            Some(o) => (o.id.clone(), o.path.clone(), o.line),
            None => (Bytes::new(), path.clone(), n as u32),
        };
        if let Some(h) = out.last_mut()
            && h.origin == id
            && h.orig_path == op
            && h.orig_start + h.len == ol
            && h.start + h.len == n as u32
        {
            h.len += 1;
            continue;
        }
        out.push(wire::BlameHunk {
            origin: id,
            orig_path: op,
            orig_start: ol,
            start: n as u32,
            len: 1,
        });
    }
    out
}

/// `Blame` for a request with a `buffer`.
pub async fn blame_buffer(
    state: &Arc<State>,
    r: wire::WsBlameRequest,
) -> Result<PluginStream<wire::BlameResponse>, Status> {
    use futures::StreamExt as _;
    if r.buffer.len() > MAX_BUFFER {
        return Err(Status::resource_exhausted(format!(
            "the buffer exceeds {MAX_BUFFER} bytes"
        )));
    }
    if r.end_line > 0 && r.start_line == 0 {
        return Err(bad("end_line needs start_line"));
    }
    let m = mounted(state, &r.ws_id)?;
    let rev = if r.rev.is_empty() {
        "HEAD".to_owned()
    } else {
        text_arg(&r.rev, "the revision", true)?
    };
    let path = path_args(std::slice::from_ref(&r.path))?;
    if path.len() != 1 {
        return Err(bad("blame needs one path"));
    }
    let path_text = path[0].to_string_lossy().into_owned();

    // The file at the revision and its blame; a path the revision lacks has no history.
    let mut args = os(&["blame", "--json"]);
    if r.ignore_whitespace {
        args.push("-w".into());
    }
    args.push(rev.clone().into());
    args.push("--".into());
    args.push(path[0].clone());
    let blob_spec = format!("{rev}:{path_text}");
    let (blob, blamed) = match run_collect(&m, os(&["cat-file", "blob", &blob_spec]), None).await {
        Ok(blob) => (blob, run_collect(&m, args, None).await?),
        Err(e) if e.code() == tonic::Code::FailedPrecondition => (Vec::new(), Vec::new()),
        Err(e) => return Err(e),
    };
    let mut hunks = Vec::new();
    let mut commits: HashMap<Bytes, (wire::LogEntry, Option<wire::ReviewState>)> = HashMap::new();
    for line in blamed
        .split(|b| *b == b'\n')
        .filter(|l| !l.trim_ascii().is_empty())
    {
        let resp: wire::BlameResponse = decode_line("asc.v1.BlameResponse", line)?
            .transcode_to()
            .map_err(internal)?;
        let parallel = resp.reviews.len() == resp.commits.len();
        for (i, c) in resp.commits.into_iter().enumerate() {
            let review = parallel.then(|| resp.reviews[i].clone());
            commits.insert(c.id.clone(), (c, review));
        }
        hunks.extend(resp.hunks);
        if hunks.len() > MAX_LINES {
            return Err(Status::resource_exhausted("the blame has too many hunks"));
        }
    }

    let old = lines_of(&blob);
    let new = lines_of(&r.buffer);
    if old.len() > MAX_LINES || new.len() > MAX_LINES {
        return Err(Status::resource_exhausted(format!(
            "more than {MAX_LINES} lines"
        )));
    }
    let old_origins = origins_of(&hunks, old.len());
    let map = map_lines(&blob, &r.buffer, r.ignore_whitespace);
    let origins: Vec<Option<Origin>> = map
        .iter()
        .map(|m| m.and_then(|i| old_origins.get(i).cloned().flatten()))
        .collect();

    let total = new.len();
    let first = (r.start_line.max(1)) as usize;
    let last = match r.end_line {
        0 => total,
        n => (n as usize).min(total),
    };
    let path_bytes = Bytes::copy_from_slice(path_text.as_bytes());
    let all = if total == 0 || first > last {
        Vec::new()
    } else {
        hunks_of(&origins, &path_bytes, first, last)
    };
    let mut messages = Vec::new();
    for group in all.chunks(HUNKS_PER_MESSAGE) {
        let mut seen: Vec<Bytes> = Vec::new();
        let mut cs = Vec::new();
        let mut reviews = Vec::new();
        for h in group {
            if h.origin.is_empty() || seen.contains(&h.origin) {
                continue;
            }
            seen.push(h.origin.clone());
            if let Some((c, rv)) = commits.get(&h.origin) {
                cs.push(c.clone());
                reviews.push(rv.clone());
            }
        }
        messages.push(Ok(wire::BlameResponse {
            hunks: group.to_vec(),
            commits: cs,
            reviews: if reviews.iter().all(Option::is_some) {
                reviews.into_iter().flatten().collect()
            } else {
                Vec::new()
            },
        }));
    }
    let _ = MAX_OUT;
    let _ = conv_blame;
    Ok(futures::stream::iter(messages).boxed())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_stacks_document_maps_to_the_response() {
        let doc = json!({
            "version": 1,
            "trunks": [{"name": "master", "tip": "00ff", "remote": null}],
            "stacks": [{
                "trunk": "master", "base": "00ff", "behind": 3,
                "changes": [{
                    "change": "abcd", "short": "abc", "commit": "0102", "asc": "0304",
                    "parent_change": null, "branches": ["feat"], "subject": "Add x",
                    "author": {"name": "A", "email": "a@x", "time": 5, "tz": "-0530"},
                    "state": "draft", "needs_restack": true, "frozen": false, "empty": false,
                    "current": true, "worktree": null,
                    "review": {"number": 7, "url": "u", "state": "draft", "approvals": 1,
                               "checks": "none", "queue": "none", "version": 2, "reviewed_version": 1}
                }]
            }]
        });
        let r = stacks_of(&doc).unwrap();
        assert_eq!(r.version, 1);
        assert_eq!(r.trunks[0].tip.as_ref(), [0x00, 0xff]);
        assert_eq!(r.trunks[0].remote, "");
        let c = &r.stacks[0].changes[0];
        assert_eq!(r.stacks[0].behind, 3);
        assert_eq!(c.commit.as_ref(), [1, 2]);
        assert_eq!(c.author.as_ref().unwrap().tz_min, -330);
        assert!(c.needs_restack && c.current && !c.frozen);
        assert_eq!(c.branches, ["feat"]);
        let rv = c.review.as_ref().unwrap();
        assert!(rv.draft);
        assert_eq!(rv.number, 7);
        assert_eq!(rv.reviewed_version, 1);
    }

    #[test]
    fn a_malformed_document_is_internal_not_a_panic() {
        assert!(stacks_of(&json!({"version": 1})).is_err());
        assert!(stacks_of(&json!({"version": 1, "trunks": 3, "stacks": []})).is_err());
    }

    #[test]
    fn zones_parse_like_git() {
        assert_eq!(tz_minutes("+0530"), 330);
        assert_eq!(tz_minutes("-0800"), -480);
        assert_eq!(tz_minutes("junk"), 0);
    }

    #[test]
    fn buffer_lines_map_to_the_blob_by_a_line_diff() {
        let map = map_lines(b"a\nb\nc\n", b"a\nX\nb\nc\n", false);
        assert_eq!(map, [Some(0), None, Some(1), Some(2)]);
        assert_eq!(map_lines(b"a  b\n", b"a b\n", true), [Some(0)]);
        assert_eq!(map_lines(b"a  b\n", b"a b\n", false), [None]);
        assert_eq!(map_lines(b"", b"x\ny\n", false), [None, None]);
        assert!(map_lines(b"x\n", b"", false).is_empty());
    }

    #[test]
    fn hunks_merge_consecutive_lines_and_mark_new_ones_uncommitted() {
        let o = |l: u32| {
            Some(Origin {
                id: Bytes::from_static(b"c1"),
                path: Bytes::from_static(b"f"),
                line: l,
            })
        };
        let origins = vec![o(1), o(2), None, o(3)];
        let h = hunks_of(&origins, &Bytes::from_static(b"f"), 1, 4);
        assert_eq!(h.len(), 3);
        assert_eq!((h[0].start, h[0].len, h[0].orig_start), (1, 2, 1));
        assert!(h[1].origin.is_empty());
        assert_eq!((h[1].start, h[1].len), (3, 1));
        assert_eq!((h[2].start, h[2].orig_start), (4, 3));
        let part = hunks_of(&origins, &Bytes::from_static(b"f"), 2, 3);
        assert_eq!(part.len(), 2);
    }
}
