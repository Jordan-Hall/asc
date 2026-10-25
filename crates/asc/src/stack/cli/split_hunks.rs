//! `asc split [--change <rev>] --hunks <spec>` (SPEC §11.2, §24.8.2 A8): splits one change into a
//! stack from a `SplitSpec`, with no prompt. The spec file is the proto3 JSON of
//! `SplitSpec { pieces }` (`cli.proto`, M02). Each piece is
//! `{"message": "...", "patch": "<base64 git-apply patch>", "wholeFiles": ["path"]}`, bottom first.
//!
//! * `patch` is applied to the tree of the piece below (the base for the first piece) with the
//!   engine's `git apply` rules (strict context, no fuzz). A patch that does not apply, names a
//!   path the change does not touch, or is a binary, symlink or submodule patch refuses the whole
//!   split; nothing is changed.
//! * `wholeFiles` take the change's full edit of each named file (binary files, renames).
//! * The last piece takes the remainder: it has the tree of the original change, keeps its change
//!   id, and takes the original message unless the piece names one. It carries no patch.
//!
//! The interactive `--by-hunk` is unchanged and is never run by a plugin.

use std::collections::BTreeMap;
use std::io::{Cursor, Read as _};
use std::path::Path;
use std::process::ExitCode;

use asc_core::change::RewriteOp;
use asc_core::content::ingest;
use asc_core::git::ObjectKind;
use asc_core::id::{ChangesetId, FileId, TreeId};
use asc_core::object::{Change, Kind};
use asc_core::path::{RepoPath, checkout_safe};
use asc_engine::derive::GitMapRow;
use asc_engine::diff::apply::{self, ApplyOptions, MAX_MATCH_WORK, ParseOptions, Work};
use asc_state::state::git_map;
use serde::Deserialize;

use super::info::resolve_change;
use super::rewrite::{Made, Piece, emit, finish, locate, record, refuse_in_progress, write_piece};
use super::sl::{Local, clean};
use super::split::Args;
use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::cli::restore::root_of;

/// Longest spec file read.
const MAX_SPEC: u64 = 64 * 1024 * 1024;
/// Most pieces one split makes.
const MAX_PIECES: usize = 64;
/// Longest message of one piece.
const MAX_MESSAGE: usize = 1024 * 1024;
/// Most whole files in one piece.
const MAX_WHOLE: usize = 100_000;

/// A string, or `{"b64": ...}` for bytes that are not UTF-8 (the asc JSON profile).
#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum Text {
    Plain(String),
    B64 { b64: String },
}

impl Text {
    fn bytes(&self) -> Option<Vec<u8>> {
        match self {
            Text::Plain(s) => Some(s.as_bytes().to_vec()),
            Text::B64 { b64 } => b64_decode(b64),
        }
    }
}

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct SplitPiece {
    #[serde(default)]
    message: Option<Text>,
    /// Standard base64 of a git-apply patch.
    #[serde(default)]
    patch: Option<String>,
    #[serde(default, alias = "whole_files", rename = "wholeFiles")]
    whole_files: Vec<Text>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct SplitSpec {
    pieces: Vec<SplitPiece>,
}

fn b64_val(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Standard base64 (proto3 JSON `bytes`); `None` on anything else.
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in s {
        acc = (acc << 6) | u32::from(b64_val(c)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xff).ok()?);
        }
    }
    Some(out)
}

fn bad(m: impl std::fmt::Display) -> CliError {
    CliError::Raw(format!("asc: split: {m}; nothing was changed"), 1)
}

fn show(path: &[u8]) -> String {
    clean(&String::from_utf8_lossy(path))
}

fn read_spec(file: &Path) -> Result<SplitSpec> {
    let meta = std::fs::symlink_metadata(file).map_err(|_| bad("the spec file cannot be read"))?;
    if !meta.is_file() || meta.len() > MAX_SPEC {
        return Err(bad("the spec is not a small regular file"));
    }
    let mut s = String::new();
    std::fs::File::open(file)
        .and_then(|f| f.take(MAX_SPEC).read_to_string(&mut s))
        .map_err(|_| bad("the spec file cannot be read"))?;
    let spec: SplitSpec = serde_json::from_str(&s).map_err(|e| {
        bad(format!(
            "the spec is not a valid SplitSpec ({})",
            clean(&e.to_string())
        ))
    })?;
    if spec.pieces.len() < 2 {
        return Err(bad("a split needs at least two pieces"));
    }
    if spec.pieces.len() > MAX_PIECES {
        return Err(bad("too many pieces"));
    }
    let last = spec.pieces.len() - 1;
    for (i, p) in spec.pieces.iter().enumerate() {
        if p.whole_files.len() > MAX_WHOLE {
            return Err(bad("too many whole files"));
        }
        if let Some(m) = &p.message
            && m.bytes().is_none_or(|b| b.len() > MAX_MESSAGE)
        {
            return Err(bad(format!(
                "the message of piece {} is invalid or too long",
                i + 1
            )));
        }
        if i == last
            && (p.patch.as_deref().is_some_and(|s| !s.is_empty()) || !p.whole_files.is_empty())
        {
            return Err(bad(
                "the last piece takes the remainder and carries no patch or whole files",
            ));
        }
    }
    Ok(spec)
}

fn checked(path: &[u8]) -> Result<RepoPath> {
    RepoPath::new(path.to_vec())
        .ok()
        .filter(checkout_safe)
        .ok_or_else(|| bad(format!("invalid path '{}'", show(path))))
}

/// The regular file `path` in `root` (content and kind), or `None` when absent.
async fn read_at(ws: &WsCtx, root: TreeId, path: &RepoPath) -> Result<Option<(Vec<u8>, Kind)>> {
    let Some(e) = ws.engine.lookup(root, path).await? else {
        return Ok(None);
    };
    if !matches!(e.kind, Kind::File | Kind::Exec) {
        return Err(bad(format!(
            "'{}' is not a regular file",
            show(path.as_bytes())
        )));
    }
    let bytes = ws
        .engine
        .read_file(FileId::from_bytes(e.id), e.size)
        .await
        .map_err(|e| bad(format!("'{}': {e}", show(path.as_bytes()))))?;
    Ok(Some((bytes.to_vec(), e.kind)))
}

/// One file as a piece leaves it: `None` is deleted.
type Overlay = BTreeMap<Vec<u8>, Option<(Vec<u8>, Kind)>>;

/// Applies the patch of one piece onto `root` and adds the leaf changes to `out`. Every path the
/// patch names must be one the change touches.
async fn patch_changes(
    ctx: &Ctx,
    ws: &WsCtx,
    root: TreeId,
    patch: &[u8],
    touched: &BTreeMap<Vec<u8>, Change>,
    work: &Work,
    out: &mut BTreeMap<Vec<u8>, Change>,
) -> Result<()> {
    let files = apply::parse(patch, &ParseOptions::default())
        .map_err(|e| bad(format!("the patch: {}", clean(&e.to_string()))))?;
    if files.is_empty() {
        return Err(bad("a patch has no file"));
    }
    let mut overlay: Overlay = BTreeMap::new();
    for fp in &files {
        let src = fp.old_path.as_deref();
        let Some(dest) = fp.new_path.as_deref().or(src) else {
            return Err(bad("a file patch names no path"));
        };
        for p in [src, Some(dest)].into_iter().flatten() {
            checked(p)?;
            if !touched.contains_key(p) {
                return Err(bad(format!("'{}' is not touched by the change", show(p))));
            }
        }
        if [fp.old_mode, fp.new_mode]
            .iter()
            .flatten()
            .any(|m| matches!(m & 0o170000, 0o120000 | 0o160000))
        {
            return Err(bad(format!(
                "'{}': symbolic link and submodule patches are not supported",
                show(dest)
            )));
        }
        if fp.binary.is_some() || fp.binary_no_body {
            return Err(bad(format!(
                "'{}': a binary file goes in wholeFiles",
                show(dest)
            )));
        }
        let base = match src {
            Some(p) if !fp.is_new => {
                let cur = match overlay.get(p) {
                    Some(slot) => slot.clone(),
                    None => read_at(ws, root, &checked(p)?).await?,
                };
                if cur.is_none() {
                    return Err(bad(format!("'{}': does not exist", show(p))));
                }
                cur
            }
            _ => None,
        };
        if (fp.is_new || fp.is_rename || fp.is_copy) && (fp.is_new || src != Some(dest)) {
            let exists = match overlay.get(dest) {
                Some(slot) => slot.is_some(),
                None => read_at(ws, root, &checked(dest)?).await?.is_some(),
            };
            if exists {
                return Err(bad(format!("'{}': already exists", show(dest))));
            }
        }
        let r = apply::apply_file(
            base.as_ref().map(|b| &b.0[..]),
            fp,
            ApplyOptions::default(),
            work,
        )
        .map_err(|e| {
            bad(format!(
                "'{}': patch does not apply ({})",
                show(dest),
                clean(&e.to_string())
            ))
        })?;
        let kind = match fp.new_mode {
            Some(m) if m & 0o111 != 0 => Kind::Exec,
            Some(_) => Kind::File,
            None => base.as_ref().map_or(Kind::File, |b| b.1),
        };
        if fp.is_rename
            && let Some(o) = src
            && o != dest
        {
            overlay.insert(o.to_vec(), None);
        }
        overlay.insert(dest.to_vec(), r.content.map(|c| (c, kind)));
    }
    for (path, slot) in overlay {
        let change = match slot {
            None => Change::Delete,
            Some((content, kind)) => {
                let got = ingest(Cursor::new(&content[..]))
                    .map_err(|e| bad(format!("'{}': {e}", show(&path))))?;
                crate::cli::add::store(ctx, ws, &got).await?;
                git_map::insert_many(
                    &ws.db,
                    ws.repo.key,
                    &[GitMapRow {
                        git: got.git,
                        kind: ObjectKind::Blob,
                        asc: *got.file.as_bytes(),
                    }],
                )
                .await?;
                Change::Write {
                    kind,
                    file: got.file,
                    size: got.size,
                    git: got.git,
                    copy_from: None,
                }
            }
        };
        if out.insert(path.clone(), change).is_some() {
            return Err(bad(format!(
                "'{}' is named twice in one piece",
                show(&path)
            )));
        }
    }
    Ok(())
}

fn with_newline(mut s: Vec<u8>) -> Vec<u8> {
    if !s.ends_with(b"\n") {
        s.push(b'\n');
    }
    s
}

pub async fn run(ctx: &Ctx, a: Args, file: &Path) -> Result<ExitCode> {
    if !a.by_file.is_empty() || a.by_hunk || a.by_commit || !a.message.is_empty() {
        return Err(CliError::Usage(
            "asc split: --hunks takes no --by-file, --by-hunk, --by-commit or -m".into(),
        ));
    }
    let spec = read_spec(file)?;
    let l = Local::open(ctx).await?;
    let ws = l.ws;
    refuse_in_progress(ws, "split")?;
    let st = l.stacks().await?;
    let (x, _) = resolve_change(ctx, &l, &st, a.change.as_deref()).await?;
    let loc = locate(&st, x, "split").await?;
    drop(st);
    let cx = ws.engine.get_changeset(x).await?;
    if cx.parents.len() != 1 {
        return Err(CliError::Raw("asc: split: the change is a merge".into(), 1));
    }
    let base = cx.parents[0];
    let base_root = root_of(ws, Some(base)).await?;

    // Everything the change edits, by path.
    let touched: BTreeMap<Vec<u8>, Change> = ws
        .engine
        .leafdiff(base_root, cx.root, None)
        .await?
        .into_iter()
        .map(|(p, c)| (p.as_bytes().to_vec(), c))
        .collect();

    let work = Work::new(MAX_MATCH_WORK);
    let lower = spec.pieces.len() - 1;
    let mut trees = Vec::with_capacity(spec.pieces.len());
    let mut cur = base_root;
    let mut whole_seen: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
    for (i, p) in spec.pieces[..lower].iter().enumerate() {
        let mut changes: BTreeMap<Vec<u8>, Change> = BTreeMap::new();
        for w in &p.whole_files {
            let path = w
                .bytes()
                .ok_or_else(|| bad("a whole file path is not valid"))?;
            checked(&path)?;
            let change = touched
                .get(&path)
                .ok_or_else(|| bad(format!("'{}' is not touched by the change", show(&path))))?;
            if whole_seen.insert(path.clone(), i).is_some() {
                return Err(bad(format!("'{}' is in two pieces", show(&path))));
            }
            changes.insert(path, change.clone());
        }
        if let Some(patch) = p.patch.as_deref().filter(|s| !s.is_empty()) {
            let patch = b64_decode(patch).ok_or_else(|| bad("a patch is not base64"))?;
            let mut from_patch = BTreeMap::new();
            patch_changes(ctx, ws, cur, &patch, &touched, &work, &mut from_patch).await?;
            for (path, c) in from_patch {
                if changes.insert(path.clone(), c).is_some() {
                    return Err(bad(format!(
                        "'{}' is in the patch and in wholeFiles of piece {}",
                        show(&path),
                        i + 1
                    )));
                }
            }
        }
        let list: Vec<_> = changes
            .into_iter()
            .map(|(p, c)| checked(&p).map(|rp| (rp, c)))
            .collect::<Result<_>>()?;
        let next = ws.engine.apply(cur, &list).await?;
        if next == cur {
            return Err(bad(format!("piece {} changes nothing", i + 1)));
        }
        cur = next;
        trees.push(cur);
    }
    if cur == cx.root {
        return Err(bad("nothing is left for the last piece"));
    }
    trees.push(cx.root);

    let op_id = uuid::Uuid::now_v7();
    let sign = ctx.config.bool("commit.gpgsign") == Some(true);
    let subject =
        String::from_utf8_lossy(cx.message.split(|&b| b == b'\n').next().unwrap_or_default())
            .into_owned();
    let mut prev = base;
    let mut made: Vec<Made> = Vec::new();
    for (i, root) in trees.iter().copied().enumerate() {
        let top = i == lower;
        let named = spec.pieces[i]
            .message
            .as_ref()
            .and_then(Text::bytes)
            .filter(|m| !m.iter().all(u8::is_ascii_whitespace));
        let message = match (named, top) {
            (Some(m), _) => with_newline(m),
            (None, true) => cx.message.clone(),
            (None, false) => {
                with_newline(format!("{subject} (split {}/{})", i + 1, lower + 1).into_bytes())
            }
        };
        let new = write_piece(
            ctx,
            ws,
            Piece {
                parent: prev,
                root,
                author: cx.author.clone(),
                message,
            },
            sign,
        )
        .await?;
        made.push(record(ws, new, &[x], RewriteOp::Split, Some(top), op_id).await?);
        prev = new;
    }
    let pairs: Vec<(ChangesetId, ChangesetId)> = vec![(x, prev)];
    let flow = finish(ctx, ws, op_id, pairs, loc.upper.clone(), prev).await?;
    emit(ctx, a.json, "split", "Split", &made, 1, &flow)
}
