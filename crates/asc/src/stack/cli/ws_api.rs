//! `asc ws-api worktrees|conflicts|graph` (SPEC §20.6, C31): the local producers behind the
//! `Workspace.Worktrees`, `Workspace.Conflicts` and `Workspace.CommitGraph` plugin RPCs. The daemon
//! runs this verb in the mounted workspace, as it runs `asc status --json`, and decodes the JSON
//! lines (the asc JSON profile) into the typed response, so an RPC answers exactly what the CLI
//! prints. It is a plumbing verb for the daemon and for tests; it is not in the user documentation.
//!
//! Every answer is bounded: 10,000 worktrees or conflicts, a 4 MiB marked file, 2,000 graph rows
//! per page, and 16 `diffstat.v1` derivations per call. Nothing here writes to the object store
//! beyond the memos `diffstat.v1` derives, and a work tree file is read only through `SafeRoot`.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::process::ExitCode;

use asc_core::id::{ChangesetId, FileId, RefTarget};
use asc_core::object::Kind;
use asc_core::path::{RefName, RepoPath};
use asc_engine::commit_graph::{DEFAULT_PAGE, MAX_PAGE, Start};
use asc_engine::graph::{EngineSource, Graph, LogQuery};
use asc_engine::history::HistoryApi;
use asc_rpc::server::wire;
use asc_state::state::stage::{self, StageSet};
use asc_state::state::workspaces;
use asc_store::refs::Refs;
use asc_wc::SafeRoot;
use futures::TryStreamExt;
use prost_reflect::{DescriptorPool, DynamicMessage};

use crate::cli::add::target::{Staged, Target};
use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::cli::json::JsonLines;
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "ws-api", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// `worktrees`, `conflicts`, `graph` or `read`.
    pub what: String,
    #[arg(long, action = clap::ArgAction::Append)]
    pub include: Vec<String>,
    #[arg(long, action = clap::ArgAction::Append)]
    pub exclude: Vec<String>,
    /// `0` all, `1` current, `2` smart, `3` stack (the `GraphPreset` numbers).
    #[arg(long, default_value_t = 0)]
    pub preset: i32,
    #[arg(long)]
    pub lanes: bool,
    #[arg(long)]
    pub stats: bool,
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
    /// The page token, hex.
    #[arg(long, default_value = "")]
    pub page_token: String,
    /// Add a WIP row per worktree with uncommitted work (first page only).
    #[arg(long)]
    pub wip: bool,
    /// `read`: the revision (default `HEAD`).
    #[arg(long)]
    pub rev: Option<String>,
    /// `read`: the repository path (with `--rev` or `--stage`).
    #[arg(long, allow_hyphen_values = true)]
    pub path: Option<String>,
    /// `read`: a `FileId`, 64 hex digits.
    #[arg(long)]
    pub file_id: Option<String>,
    /// `read`: a stage, 0 to 3 (with `--path`).
    #[arg(long)]
    pub stage: Option<u8>,
    /// `read`: the first byte.
    #[arg(long, default_value_t = 0)]
    pub offset: u64,
    /// `read`: the most bytes; 0 reads to the end.
    #[arg(long, default_value_t = 0)]
    pub length: u64,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "ws-api",
    flags: &[
        "--include",
        "--exclude",
        "--preset",
        "--lanes",
        "--stats",
        "--limit",
        "--page-token",
        "--wip",
        "--rev",
        "--path",
        "--file-id",
        "--stage",
        "--offset",
        "--length",
    ],
    placeholders: &[],
}];

super::verb!(WS_API, "ws-api");

/// Most worktrees or conflicts one answer lists.
const MAX_ITEMS: usize = 10_000;
/// Largest marked file returned inline.
const MAX_MARKED: u64 = 4 * 1024 * 1024;
/// `diffstat.v1` derivations one graph call may start.
const STATS_DERIVE: usize = 16;
/// The ahead and behind counts stop here.
const COUNT_CAP: usize = 999;
/// Longest page token (hex digits).
const MAX_TOKEN: usize = 8192;
/// Most revisions in `include` or `exclude`.
const MAX_REVS: usize = 64;
/// Most refs a preset expands to.
const MAX_REFS: usize = 100_000;
/// Rows per `CommitGraphResponse` line.
const ROWS_PER_LINE: usize = 100;

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("asc: {e}"))
}

fn usage(e: impl std::fmt::Display) -> CliError {
    CliError::Usage(format!("asc: {e}"))
}

fn hex_of(b: &[u8]) -> String {
    use std::fmt::Write as _;
    b.iter()
        .fold(String::with_capacity(b.len() * 2), |mut s, x| {
            let _ = write!(s, "{x:02x}");
            s
        })
}

fn unhex(t: &str) -> Option<Vec<u8>> {
    if !t.len().is_multiple_of(2) || !t.is_ascii() {
        return None;
    }
    (0..t.len() / 2)
        .map(|i| u8::from_str_radix(&t[2 * i..2 * i + 2], 16).ok())
        .collect()
}

/// One message as a JSON line of the asc JSON profile.
macro_rules! put {
    ($out:expr, $pool:expr, $name:literal, $msg:expr) => {{
        let desc = $pool
            .get_message_by_name($name)
            .ok_or_else(|| fatal(concat!("missing message ", $name)))?;
        let mut d = DynamicMessage::new(desc);
        d.transcode_from(&$msg).map_err(fatal)?;
        $out.emit(&d).map_err(fatal)?;
    }};
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    if a.what == "read" {
        return read(ctx, ws, &a).await;
    }
    let pool = DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).map_err(fatal)?;
    let stdout = std::io::stdout();
    let mut out = JsonLines::new(std::io::BufWriter::new(stdout.lock()));
    match a.what.as_str() {
        "worktrees" => {
            let list = worktrees(ctx, ws).await?;
            let resp = wire::WsWorktreesResponse {
                worktrees: list.into_iter().map(|w| w.wire).collect(),
            };
            put!(out, pool, "asc.v1.WsWorktreesResponse", resp);
        }
        "conflicts" => {
            let resp = wire::WsConflictsResponse {
                conflicts: conflicts(ws).await?,
            };
            put!(out, pool, "asc.v1.WsConflictsResponse", resp);
        }
        "graph" => graph(ctx, ws, &a, &pool, &mut out).await?,
        other => return Err(usage(format!("ws-api: unknown query '{other}'"))),
    }
    out.finish()?;
    Ok(ExitCode::SUCCESS)
}

// ------------------------------------------------------------------------------ worktrees

/// A worktree row and the changeset its HEAD names (for WIP rows).
struct Wt {
    wire: wire::WsWorktree,
    head: Option<ChangesetId>,
}

fn in_progress(gitdir: &std::path::Path) -> &'static str {
    let has = |n: &str| gitdir.join(n).exists();
    if has("rebase-merge") || has("rebase-apply") {
        "rebase"
    } else if has("MERGE_HEAD") {
        "merge"
    } else if has("CHERRY_PICK_HEAD") {
        "cherry-pick"
    } else if has("REVERT_HEAD") {
        "revert"
    } else if has("BISECT_LOG") {
        "bisect"
    } else {
        ""
    }
}

async fn tip(ws: &WsCtx, name: &str) -> Result<Option<ChangesetId>> {
    let Ok(name) = RefName::new(name.as_bytes().to_vec()) else {
        return Ok(None);
    };
    let got = ws.db.refs().get(ws.repo.key, &name).await?;
    Ok(got.and_then(|e| match e.target {
        RefTarget::Changeset(c) => Some(c),
        RefTarget::Tag(_) => None,
    }))
}

async fn count(ws: &WsCtx, include: ChangesetId, exclude: ChangesetId) -> Result<u32> {
    let g = Graph::new(EngineSource::new(&ws.engine));
    let q = LogQuery {
        include: vec![include],
        exclude: vec![exclude],
        limit: Some(COUNT_CAP + 1),
        ..LogQuery::default()
    };
    let ids: Vec<ChangesetId> = g.log(q).try_collect().await.map_err(fatal)?;
    Ok(ids.len().min(COUNT_CAP) as u32)
}

async fn worktrees(ctx: &Ctx, ws: &WsCtx) -> Result<Vec<Wt>> {
    let rows = workspaces::list(&ws.db, Some(ws.repo.key)).await?;
    if rows.len() > MAX_ITEMS {
        return Err(fatal(format!("more than {MAX_ITEMS} worktrees")));
    }
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let id = r.ws.id;
        let branch =
            r.ws.head
                .strip_prefix("refs/heads/")
                .unwrap_or(&r.ws.head)
                .to_owned();
        let head = if id == ws.workspace.ws.id {
            ws.head_commit().await?
        } else if branch.is_empty() {
            r.ws.base_cs
        } else {
            tip(ws, &format!("refs/heads/{branch}")).await?
        };
        let git = match head {
            Some(h) => ws
                .engine
                .git_commit_of(h)
                .await
                .map_err(fatal)?
                .as_bytes()
                .to_vec(),
            None => Vec::new(),
        };
        let dirty: i64 =
            sqlx::query_scalar("SELECT COUNT(DISTINCT path) FROM journal WHERE ws_id = ?")
                .bind(id.to_string())
                .fetch_one(ws.db.pool())
                .await
                .map_err(fatal)?;
        let staged = stage::overlay(&ws.db, id, StageSet::Default)
            .await?
            .iter()
            .filter(|s| s.stage == 0)
            .count();
        let (mut ahead, mut behind) = (0, 0);
        if let (false, Some(h)) = (branch.is_empty(), head)
            && let Some((_, tracking)) = crate::cli::switch::upstream_ref(ctx, &branch)
            && let Some(up) = tip(ws, &tracking).await?
        {
            ahead = count(ws, h, up).await?;
            behind = count(ws, up, h).await?;
        }
        out.push(Wt {
            wire: wire::WsWorktree {
                ws_id: id.to_string(),
                path: r.ws.path.clone(),
                mount: r.ws.mount.clone(),
                mode: match r.ws.mode {
                    workspaces::Mode::Virtual => "virtual",
                    workspaces::Mode::Plain => "plain",
                }
                .to_owned(),
                head_ref: if branch.is_empty() {
                    String::new()
                } else {
                    format!("refs/heads/{branch}")
                },
                head: git.into(),
                dirty: u32::try_from(dirty).unwrap_or(u32::MAX),
                staged: u32::try_from(staged).unwrap_or(u32::MAX),
                ahead,
                behind,
                ephemeral_until: r.ws.ephemeral_until.unwrap_or(0),
                locked: r.locked.clone().unwrap_or_default(),
                in_progress: in_progress(&ws.home.gitdir(id)).to_owned(),
            },
            head,
        });
    }
    Ok(out)
}

// ------------------------------------------------------------------------------ conflicts

/// The marked file of a conflicted path, or nothing when it is missing, a link, not a regular
/// file or too large.
fn marked(ws: &WsCtx, path: &[u8]) -> Vec<u8> {
    let Ok(rp) = RepoPath::new(path.to_vec()) else {
        return Vec::new();
    };
    let gate = SafeRoot::new(ws.projection.toplevel());
    let Some(fs) = gate.resolve(&rp) else {
        return Vec::new();
    };
    let Ok(meta) = std::fs::symlink_metadata(&fs) else {
        return Vec::new();
    };
    if !meta.is_file() || meta.len() > MAX_MARKED {
        return Vec::new();
    }
    let mut buf = Vec::new();
    match std::fs::File::open(&fs) {
        Ok(f) => {
            if f.take(MAX_MARKED + 1).read_to_end(&mut buf).is_err()
                || buf.len() as u64 > MAX_MARKED
            {
                buf.clear();
            }
        }
        Err(_) => buf.clear(),
    }
    buf
}

async fn conflicts(ws: &WsCtx) -> Result<Vec<wire::WsConflict>> {
    let rows = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    type Sides = [Option<Vec<u8>>; 3];
    let mut by_path: Vec<(Vec<u8>, Sides)> = Vec::new();
    for r in rows.iter().filter(|r| (1..=3).contains(&r.stage)) {
        if by_path.last().is_none_or(|(p, _)| *p != r.path) {
            if by_path.len() >= MAX_ITEMS {
                return Err(fatal(format!("more than {MAX_ITEMS} conflicted paths")));
            }
            by_path.push((r.path.clone(), [None, None, None]));
        }
        if let Some((_, slots)) = by_path.last_mut() {
            slots[usize::from(r.stage) - 1] =
                Some(r.git.map(|g| g.as_bytes().to_vec()).unwrap_or_default());
        }
    }
    let mut out = Vec::with_capacity(by_path.len());
    for (path, [base, ours, theirs]) in by_path {
        let kind = match (&base, &ours, &theirs) {
            (None, Some(_), Some(_)) => wire::ConflictKind::AddAdd,
            (Some(_), Some(_), None) | (Some(_), None, Some(_)) => wire::ConflictKind::ModifyDelete,
            _ => wire::ConflictKind::Content,
        };
        let marked = marked(ws, &path);
        out.push(wire::WsConflict {
            path: path.into(),
            kind: kind as i32,
            base: base.unwrap_or_default().into(),
            ours: ours.unwrap_or_default().into(),
            theirs: theirs.unwrap_or_default().into(),
            marked: marked.into(),
        });
    }
    Ok(out)
}

// ------------------------------------------------------------------------------ graph

async fn rev(ws: &WsCtx, spec: &str) -> Result<ChangesetId> {
    if spec.len() > 1024
        || spec.starts_with('-')
        || spec.bytes().any(|b| b == 0 || b == b'\n' || b == b'\r')
    {
        return Err(usage(format!("bad revision {spec:?}")));
    }
    let spec = if spec.is_empty() { "HEAD" } else { spec };
    let found = if spec == "HEAD" || spec == "@" {
        ws.head_commit().await?.into_iter().collect()
    } else {
        ws.engine
            .resolve(&[spec.to_owned()])
            .await
            .map_err(fatal)?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
    };
    found
        .into_iter()
        .next()
        .ok_or_else(|| CliError::Fatal(format!("asc: unknown revision {spec:?}")))
}

/// Changeset targets of the refs under `prefix`.
async fn refs_under(ws: &WsCtx, prefix: &str) -> Result<Vec<(String, ChangesetId)>> {
    let all = ws.db.refs().list(ws.repo.key, prefix).await?;
    if all.len() > MAX_REFS {
        return Err(fatal("too many refs for this preset; pass --include"));
    }
    Ok(all
        .into_iter()
        .filter_map(|r| match r.target {
            RefTarget::Changeset(id) => {
                Some((String::from_utf8_lossy(r.name.as_bytes()).into_owned(), id))
            }
            RefTarget::Tag(_) => None,
        })
        .collect())
}

async fn starts(ctx: &Ctx, ws: &WsCtx, a: &Args) -> Result<(Vec<ChangesetId>, Vec<ChangesetId>)> {
    if a.include.len() > MAX_REVS || a.exclude.len() > MAX_REVS {
        return Err(usage("too many revisions"));
    }
    let mut include = Vec::new();
    for s in &a.include {
        include.push(rev(ws, s).await?);
    }
    let mut exclude = Vec::new();
    for s in &a.exclude {
        exclude.push(rev(ws, s).await?);
    }
    let trunk = ctx
        .config
        .get_all("asc.stack.trunk")
        .into_iter()
        .find(|t| !t.is_empty())
        .unwrap_or_else(|| "master".into());
    let trunk_ref = format!("refs/heads/{trunk}");
    match a.preset {
        3 => {
            let heads = refs_under(ws, "refs/heads/").await?;
            if let Some((_, id)) = heads.iter().find(|(n, _)| *n == trunk_ref) {
                exclude.push(*id);
            }
            if include.is_empty() {
                include.extend(
                    heads
                        .iter()
                        .filter(|(n, _)| *n != trunk_ref)
                        .map(|(_, id)| *id),
                );
            }
        }
        0..=2 if include.is_empty() => match a.preset {
            1 => include.extend(ws.head_commit().await?),
            2 => {
                include.extend(ws.head_commit().await?);
                include.extend(
                    refs_under(ws, "refs/heads/")
                        .await?
                        .into_iter()
                        .map(|(_, id)| id),
                );
            }
            _ => {
                let mut all = refs_under(ws, "refs/heads/").await?;
                all.extend(refs_under(ws, "refs/tags/").await?);
                include.extend(all.into_iter().map(|(_, id)| id));
            }
        },
        0..=2 => {}
        _ => return Err(usage("unknown graph preset")),
    }
    Ok((include, exclude))
}

fn stat_wire(s: &asc_engine::derive::diffstat::DiffStatV1) -> wire::DiffStat {
    wire::DiffStat {
        files: s
            .files
            .iter()
            .map(|f| wire::FileStat {
                path: f.path.as_bytes().to_vec().into(),
                added: f.added,
                removed: f.removed,
                binary: f.binary,
            })
            .collect(),
    }
}

async fn graph<W: Write>(
    ctx: &Ctx,
    ws: &WsCtx,
    a: &Args,
    pool: &DescriptorPool,
    out: &mut JsonLines<W>,
) -> Result<()> {
    let limit = match a.limit {
        0 => DEFAULT_PAGE,
        n => n.min(MAX_PAGE),
    };
    let token = if a.page_token.is_empty() {
        None
    } else if a.page_token.len() > MAX_TOKEN {
        return Err(usage("the page token is too long"));
    } else {
        Some(unhex(&a.page_token).ok_or_else(|| usage("malformed page token"))?)
    };
    let (include, exclude) = match token {
        None => starts(ctx, ws, a).await?,
        Some(_) => (Vec::new(), Vec::new()),
    };
    let start = match &token {
        Some(t) => Start::Token(t),
        None => Start::New {
            include: &include,
            exclude: &exclude,
        },
    };
    let page = ws
        .engine
        .graph_page(start, limit)
        .await
        .map_err(|e| match e {
            asc_engine::commit_graph::Error::Expired => CliError::Fatal(
                "asc: the page token has expired; restart from the first page".into(),
            ),
            e => fatal(e),
        })?;

    // Branch and tag names of the shown changesets.
    let shown: HashSet<ChangesetId> = page.rows.iter().map(|r| r.id).collect();
    let mut labels: HashMap<ChangesetId, Vec<String>> = HashMap::new();
    if !shown.is_empty() {
        for prefix in ["refs/heads/", "refs/tags/"] {
            for (name, id) in refs_under(ws, prefix).await? {
                if shown.contains(&id) {
                    labels.entry(id).or_default().push(name);
                }
            }
        }
    }
    // Stats: a memo when one exists, a derivation while the call's budget lasts, else pending.
    let ids: Vec<ChangesetId> = page.rows.iter().map(|r| r.id).collect();
    let memos = if a.stats {
        ws.engine.diffstat_memos(&ids).await.map_err(fatal)?
    } else {
        Vec::new()
    };
    let mut budget = STATS_DERIVE;
    let mut messages: Vec<wire::GraphRow> = Vec::with_capacity(page.rows.len() + 1);
    if a.wip && token.is_none() {
        for w in worktrees(ctx, ws).await? {
            if w.wire.dirty == 0 && w.wire.staged == 0 {
                continue;
            }
            messages.push(wire::GraphRow {
                entry: Some(wire::LogEntry {
                    parents: w
                        .head
                        .iter()
                        .map(|h| h.as_bytes().to_vec().into())
                        .collect(),
                    message: b"Uncommitted changes".to_vec().into(),
                    ..wire::LogEntry::default()
                }),
                wip: true,
                ws_id: w.wire.ws_id.clone(),
                ..wire::GraphRow::default()
            });
        }
    }
    let rows = crate::stack::change_rows(ctx)?;
    for (i, row) in page.rows.iter().enumerate() {
        let cs = ws.engine.get_changeset(row.id).await.map_err(fatal)?;
        let git = ws.engine.git_commit_of(row.id).await.map_err(fatal)?;
        let stats = if a.stats {
            match memos.get(i).and_then(Option::as_ref) {
                Some(s) => Some(stat_wire(s)),
                None if budget > 0 => {
                    budget -= 1;
                    Some(stat_wire(&ws.engine.diffstat(row.id).await.map_err(fatal)?))
                }
                None => None,
            }
        } else {
            None
        };
        let change = asc_engine::change::ChangeRows::get(&rows, row.id)
            .await
            .map_err(fatal)?
            .map(|r| r.change.as_bytes().to_vec())
            .unwrap_or_default();
        let entry = wire::LogEntry {
            id: row.id.as_bytes().to_vec().into(),
            git: git.as_bytes().to_vec().into(),
            parents: cs
                .parents
                .iter()
                .map(|p| p.as_bytes().to_vec().into())
                .collect(),
            author: Some(wire::Signature::from(&cs.author)),
            committer: Some(wire::Signature::from(&cs.committer)),
            message: cs.message.to_vec().into(),
            r#gen: row.generation,
            change: change.into(),
            stats,
            ..wire::LogEntry::default()
        };
        let edges = if a.lanes {
            row.edges
                .iter()
                .map(|e| wire::GraphEdge {
                    to_column: e.to_column,
                    parent: e.parent.as_bytes().to_vec().into(),
                })
                .collect()
        } else {
            Vec::new()
        };
        messages.push(wire::GraphRow {
            entry: Some(entry),
            refs: labels.get(&row.id).cloned().unwrap_or_default(),
            column: if a.lanes { row.column } else { 0 },
            edges,
            overflow: a.lanes && row.overflow,
            ..wire::GraphRow::default()
        });
    }
    let next = page.next.as_deref().map(hex_of).unwrap_or_default();
    let chunks = messages.len().div_ceil(ROWS_PER_LINE).max(1);
    for (i, group) in messages.chunks(ROWS_PER_LINE).enumerate().chain(
        // An empty page still prints one (empty) message so the stream is never silent.
        messages.is_empty().then_some((0, &[][..])),
    ) {
        let resp = wire::CommitGraphResponse {
            rows: group.to_vec(),
            next_page_token: if i + 1 == chunks {
                next.clone()
            } else {
                String::new()
            },
        };
        put!(out, pool, "asc.v1.CommitGraphResponse", resp);
    }
    Ok(())
}

// ------------------------------------------------------------------------------ read

/// The `read` query behind `Workspace.ReadFile` (P02): the total size of the file as 8 big-endian
/// bytes, then the range `offset..offset+length` (`length` 0 is to the end) as raw bytes. The file
/// is read from the object store (hydrated through the tiers when an object is missing), never
/// from the work tree. A missing path, stage or object prints `not found:` and fails before any
/// byte is written. Hashes are checked: a whole read through the file hash, a range through the
/// hash of each chunk it touches.
async fn read(ctx: &Ctx, ws: &WsCtx, a: &Args) -> Result<ExitCode> {
    let (file, known) = read_target(ctx, ws, a).await?;
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    read_range(ws, file, known, a.offset, a.length, &mut out).await?;
    out.flush().map_err(fatal)?;
    Ok(ExitCode::SUCCESS)
}

fn not_found(what: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("asc: not found: {what}"))
}

fn read_path(a: &Args) -> Result<RepoPath> {
    let p = a.path.as_deref().ok_or_else(|| usage("read needs --path"))?;
    RepoPath::new(p.as_bytes().to_vec()).map_err(|_| usage(format!("bad path {p:?}")))
}

fn blob_of(kind: Kind) -> bool {
    matches!(kind, Kind::File | Kind::Exec | Kind::Symlink)
}

/// The file to read and, when the entry told, its size.
async fn read_target(ctx: &Ctx, ws: &WsCtx, a: &Args) -> Result<(FileId, Option<u64>)> {
    if let Some(id) = &a.file_id {
        let id: FileId = id.parse().map_err(|_| usage("bad file id"))?;
        return Ok((id, None));
    }
    if let Some(stage_no) = a.stage {
        let path = read_path(a)?;
        if stage_no > 3 {
            return Err(usage("a stage is 0 to 3"));
        }
        if stage_no == 0 {
            let target = Target::open(ctx).await?;
            return match target.staged(ws, &path).await? {
                Staged::Leaf { leaf, .. } if blob_of(leaf.kind) => Ok((leaf.file, Some(leaf.size))),
                Staged::Leaf { .. } => Err(CliError::Fatal(format!("asc: {path} is not a file"))),
                Staged::Absent | Staged::Unmerged => Err(not_found(format!("{path} at stage 0"))),
            };
        }
        let rows = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
        return rows
            .into_iter()
            .find(|r| r.stage == stage_no && r.path == path.as_bytes())
            .and_then(|r| r.file.filter(|_| blob_of(r.kind)).map(|f| (f, Some(r.size))))
            .ok_or_else(|| not_found(format!("{path} at stage {stage_no}")));
    }
    let path = read_path(a)?;
    let spec = a.rev.as_deref().unwrap_or("");
    let cs = rev(ws, spec).await?;
    let root = ws.engine.get_changeset(cs).await.map_err(fatal)?.root;
    let entry = ws
        .engine
        .lookup(root, &path)
        .await
        .map_err(fatal)?
        .ok_or_else(|| not_found(format!("{path} at {}", if spec.is_empty() { "HEAD" } else { spec })))?;
    if !blob_of(entry.kind) {
        return Err(CliError::Fatal(format!("asc: {path} is not a file")));
    }
    Ok((FileId::from_bytes(entry.id), Some(entry.size)))
}

async fn read_range(
    ws: &WsCtx,
    file: FileId,
    known: Option<u64>,
    offset: u64,
    length: u64,
    out: &mut dyn Write,
) -> Result<()> {
    use asc_core::codec::decode_envelope;
    use asc_core::content::ChunkAssembler;
    use asc_core::id::ChunkId;
    use asc_core::object::ChunkList;
    use asc_store::key::ObjKey;
    use asc_store::objects::Objects as _;

    let missing = || not_found(format!("object {file}"));
    let window = |total: u64| {
        let start = offset.min(total);
        let end = if length == 0 {
            total
        } else {
            offset.saturating_add(length).min(total)
        };
        (start, end.max(start))
    };
    if let Some(bytes) = ws.objects.get(&ObjKey::File(file)).await? {
        if FileId::of(&bytes) != file {
            return Err(CliError::Fatal(format!("asc: blob {file} is corrupt")));
        }
        let total = bytes.len() as u64;
        let (start, end) = window(total);
        out.write_all(&total.to_be_bytes()).map_err(fatal)?;
        out.write_all(&bytes[start as usize..end as usize]).map_err(fatal)?;
        return Ok(());
    }
    let raw = ws
        .objects
        .get(&ObjKey::ChunkList(file))
        .await?
        .ok_or_else(missing)?;
    let list = decode_envelope::<ChunkList>(&raw).map_err(fatal)?;
    if known.is_some_and(|k| k != list.size) {
        return Err(CliError::Fatal(format!("asc: blob {file} is corrupt")));
    }
    let total = list.size;
    let (start, end) = window(total);
    let whole = start == 0 && end == total;
    let chunks = list.chunks.clone();
    let mut assembler = if whole {
        Some(ChunkAssembler::new(list, file).map_err(fatal)?)
    } else {
        None
    };
    out.write_all(&total.to_be_bytes()).map_err(fatal)?;
    let mut pos = 0u64;
    for (id, len) in &chunks {
        let (c_start, c_end) = (pos, pos + u64::from(*len));
        pos = c_end;
        if c_end <= start || c_start >= end {
            if c_start >= end && assembler.is_none() {
                break;
            }
            continue;
        }
        let chunk = ws
            .objects
            .get(&ObjKey::Chunk(*id))
            .await?
            .ok_or_else(missing)?;
        if chunk.len() != *len as usize || ChunkId::of(&chunk) != *id {
            return Err(CliError::Fatal(format!("asc: blob {file} is corrupt")));
        }
        if let Some(asm) = assembler.as_mut() {
            asm.push(&chunk).map_err(fatal)?;
        }
        let from = start.saturating_sub(c_start) as usize;
        let to = (end.min(c_end) - c_start) as usize;
        out.write_all(&chunk[from..to]).map_err(fatal)?;
    }
    if let Some(asm) = assembler {
        asm.finish().map_err(fatal)?;
    }
    Ok(())
}
