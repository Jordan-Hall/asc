//! `asc stack get <pr|change|branch|asc:<id>> [-d] [-u] [--unfrozen] [--worktree <path>]`,
//! `asc stack freeze|unfreeze [<change>]` (SPEC §20.7, §20.3, §24.10 ruling 4).
//!
//! `get` asks the server's `Changes.Stack` for the stack around the named change, fetches the
//! heads it names (the draft heads are GC roots on the server, not refs, so the fetch goes
//! through a read-only overlay that lists one synthetic `refs/asc/stack/<change>` per head and
//! the local ref keeps the objects alive), then fetches, verifies and indexes the change
//! metadata of every head (`ChangesExt`, SPEC §20.2) before it believes the server's claim of
//! which change a head is. Fetched changes are frozen unless `--unfrozen`. The target is checked
//! out detached, in this work tree or, with `--worktree <path>`, in a new plain worktree whose
//! path must lie inside the parent directory of this work tree.
//!
//! An `asc:<id>` target is an `asc share` snapshot: `refs/snapshots/<id>` is fetched and checked
//! out; a snapshot is not a change, so nothing is frozen.
//!
//! Frozen changes are never rewritten locally: the restack driver refuses them (`drive`).

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::change::ChangeId;
use asc_core::id::{ChangesetId, RefTarget};
use asc_engine::change::ChangeRows;
use asc_engine::sync::changes::{ChangesExt, LogMeta};
use asc_engine::sync::{self, Refspec, Remote};
use asc_rpc::changes::ChangeInfo;
use asc_rpc::convert::ModeId;
use asc_rpc::server::wire;
use asc_rpc::services::changes_client::ChangesClient;
use asc_rpc::services::repo_client::RepoClient;
use asc_store::key::ObjKey;
use asc_store::refs::{RefEntry, RefUpdate};
use asc_wc::worktree as wt;
use futures::stream::BoxStream;
use serde::Serialize;

use super::info::{resolve_change, sub};
use super::sl::{Local, clean, fatal};
use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::cli::fetch::{default_remote, sync_error};
use crate::cli::output::Output;
use crate::cli::pager::PagerWant;
use crate::cli::remote::apply_refs;
use crate::cli::switch::{Dest, Move, Opts, TrackOpt, perform, ref_name, ref_target};
use crate::cli::worktree::{self, AddOpts};
use crate::remote::{self, Intent, StateBoundaries};
use crate::stack::json::{VERSION, line};

/// Most changes one `get` takes from the server (the server's own cap, `MAX_STACK`).
const MAX_CHANGES: usize = 256;
/// Longest target word.
const MAX_TARGET: usize = 512;
/// Largest reply or request message.
const MAX_MESSAGE: usize = 8 << 20;

// ------------------------------------------------------------------------------ arguments

#[derive(clap::Parser, Debug)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct GetArgs {
    /// A review number (`#12`), a change id, a branch or revision, or `asc:<snapshot id>`.
    pub target: String,
    /// Only the target and the changes below it.
    #[arg(short = 'd', long)]
    pub downstack: bool,
    /// Only the target and the changes above it.
    #[arg(short = 'u', long)]
    pub upstack: bool,
    /// Leave the fetched changes unfrozen.
    #[arg(long)]
    pub unfrozen: bool,
    /// Open the target in a new plain worktree at this path.
    #[arg(long)]
    pub worktree: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Parser, Debug)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct FreezeArgs {
    /// A change prefix or revision; default is HEAD.
    pub change: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Target {
    Snapshot(String),
    Review(u64),
    Change(ChangeId),
    Rev(String),
}

fn usage(why: &str) -> CliError {
    CliError::Usage(format!(
        "asc: stack get: {why}\nusage: asc stack get <pr|change|branch|asc:<id>> [-d] [-u] [--unfrozen] [--worktree <path>]"
    ))
}

fn parse_target(word: &str) -> Result<Target> {
    if word.is_empty() || word.len() > MAX_TARGET || word.chars().any(char::is_control) {
        return Err(usage(
            "the target is empty, too long or holds a control character",
        ));
    }
    if let Some(id) = word.strip_prefix("asc:") {
        return if id.len() == 40 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
            Ok(Target::Snapshot(id.to_ascii_lowercase()))
        } else {
            Err(usage("a snapshot is 'asc:' and 40 hex digits"))
        };
    }
    let digits = word.strip_prefix('#').unwrap_or(word);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        return digits
            .parse::<u64>()
            .map(Target::Review)
            .map_err(|_| usage("the review number is too large"));
    }
    if word.starts_with('-') {
        return Err(usage("the target looks like an option"));
    }
    if word.len() == ChangeId::LEN
        && let Ok(c) = ChangeId::from_str(word)
    {
        return Ok(Target::Change(c));
    }
    Ok(Target::Rev(word.to_owned()))
}

// ------------------------------------------------------------------------------ selection

/// One change of the server's answer that `get` brings in.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pick {
    change: ChangeId,
    head: ChangesetId,
}

/// Which of `infos` (bottom to top, forks included) to take. Landed changes are on the trunk
/// and never taken. With an anchor, `down` keeps it and its ancestors, `up` keeps it and its
/// descendants, both keep the union and neither keeps the whole answer. Every head must be in
/// the asc id space and every change must appear once.
fn select(
    infos: &[ChangeInfo],
    anchor: Option<ChangeId>,
    down: bool,
    up: bool,
) -> Result<Vec<Pick>> {
    if infos.len() > MAX_CHANGES {
        return Err(fatal(format!(
            "stack get: the server sent more than {MAX_CHANGES} changes"
        )));
    }
    let mut parent: HashMap<ChangeId, Option<ChangeId>> = HashMap::new();
    let mut heads: HashMap<ChangeId, ChangesetId> = HashMap::new();
    for i in infos {
        let ModeId::Asc(head) = i.head else {
            return Err(fatal(
                "stack get: the server sent a git id where an asc id was asked for",
            ));
        };
        if heads.insert(i.change, head).is_some() {
            return Err(fatal("stack get: the server named one change twice"));
        }
        parent.insert(i.change, i.parent_change);
    }
    let landed: HashSet<ChangeId> = infos
        .iter()
        .filter(|i| i.landed_as.is_some())
        .map(|i| i.change)
        .collect();
    let mut keep: HashSet<ChangeId> = infos.iter().map(|i| i.change).collect();
    if let Some(a) = anchor {
        if !heads.contains_key(&a) {
            return Err(fatal(
                "stack get: the server's answer does not hold the change asked for",
            ));
        }
        if landed.contains(&a) {
            return Err(CliError::Raw(
                format!("asc: stack get: {} has already landed", a.short()),
                1,
            ));
        }
        if down || up {
            let mut chosen: HashSet<ChangeId> = HashSet::new();
            if down {
                let mut at = Some(a);
                while let Some(c) = at {
                    if !heads.contains_key(&c) || !chosen.insert(c) {
                        break;
                    }
                    at = parent.get(&c).copied().flatten();
                }
            }
            if up {
                chosen.insert(a);
                // One pass per level is enough: the answer holds at most MAX_CHANGES changes.
                for _ in 0..infos.len() {
                    let before = chosen.len();
                    for i in infos {
                        if i.parent_change.is_some_and(|p| chosen.contains(&p)) {
                            chosen.insert(i.change);
                        }
                    }
                    if chosen.len() == before {
                        break;
                    }
                }
            }
            keep = chosen;
        }
    }
    Ok(infos
        .iter()
        .filter(|i| keep.contains(&i.change) && !landed.contains(&i.change))
        .map(|i| Pick {
            change: i.change,
            head: heads[&i.change],
        })
        .collect())
}

/// The metadata index must agree with what the server claimed for every pick: a head whose
/// verified row is missing, or names another change, stops the command before anything is frozen.
async fn verify_rows(rows: &impl ChangeRows, picks: &[Pick]) -> Result<()> {
    for p in picks {
        match rows.get(p.head).await.map_err(fatal)? {
            None => {
                return Err(fatal(format!(
                    "stack get: the server sent no change metadata for {}",
                    p.change.short()
                )));
            }
            Some(r) if r.change != p.change => {
                return Err(fatal(format!(
                    "stack get: the server's metadata names another change than {}",
                    p.change.short()
                )));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------ overlay

/// A read-only view of a remote that lists `extra` refs and nothing else, so `fetch` can pull
/// heads the server holds without a ref.
struct Overlay<'a> {
    inner: &'a dyn Remote,
    extra: Vec<RefEntry>,
}

fn read_only<T>() -> sync::Result<T> {
    Err(sync::Error::Remote("stack get reads only".into()))
}

#[async_trait]
impl Remote for Overlay<'_> {
    fn name(&self) -> &str {
        self.inner.name()
    }
    async fn list_refs(&self) -> sync::Result<Vec<RefEntry>> {
        Ok(self.extra.clone())
    }
    async fn has_many(&self, keys: &[ObjKey]) -> sync::Result<Vec<bool>> {
        self.inner.has_many(keys).await
    }
    fn get_many(&self, keys: Vec<ObjKey>) -> BoxStream<'_, sync::Result<(ObjKey, Bytes)>> {
        self.inner.get_many(keys)
    }
    async fn put_many(&self, _objects: Vec<(ObjKey, Bytes)>) -> sync::Result<()> {
        read_only()
    }
    async fn update_refs(&self, _updates: &[RefUpdate]) -> sync::Result<()> {
        read_only()
    }
    fn history(&self) -> Option<std::sync::Arc<dyn asc_engine::history::HistoryApi>> {
        self.inner.history()
    }
}

// ------------------------------------------------------------------------------ server calls

fn status(s: &tonic::Status) -> CliError {
    fatal(format!("stack get: {}", clean(s.message())))
}

fn repo_ref(conn: &remote::server::Wire) -> Option<wire::RepoRef> {
    Some(wire::RepoRef {
        name: conn.repo.clone(),
        id_mode: asc_rpc::IdMode::Asc.to_wire(),
    })
}

fn changes_client(conn: &remote::server::Wire) -> ChangesClient<tonic::transport::Channel> {
    ChangesClient::new(conn.channel.clone())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE)
}

/// The `Changes.Stack` answer for `at`, and the change the target names when it is one.
async fn ask_stack(
    conn: &remote::server::Wire,
    target: &Target,
) -> Result<(Vec<ChangeInfo>, Option<ChangeId>)> {
    use wire::stack_request::At;
    let mut client = changes_client(conn);
    let (at, anchor) = match target {
        Target::Change(c) => (At::Change(Bytes::copy_from_slice(c.as_bytes())), Some(*c)),
        Target::Review(n) => (At::Review(*n), None),
        Target::Rev(spec) => {
            let req = conn.request(
                wire::ResolveChangesRequest {
                    repo: repo_ref(conn),
                    changes: Vec::new(),
                    revs: vec![spec.clone()],
                },
                None,
            );
            let got = client
                .resolve_changes(req)
                .await
                .map_err(|s| status(&s))?
                .into_inner();
            let infos = asc_rpc::changes::change_infos(&got.changes).map_err(fatal)?;
            let Some(first) = infos.first() else {
                return Err(CliError::Raw(
                    format!(
                        "asc: stack get: '{}' is not a change on the server",
                        clean(spec)
                    ),
                    1,
                ));
            };
            (
                At::Change(Bytes::copy_from_slice(first.change.as_bytes())),
                Some(first.change),
            )
        }
        Target::Snapshot(_) => return Err(fatal("stack get: a snapshot has no stack")),
    };
    let req = conn.request(
        wire::StackRequest {
            repo: repo_ref(conn),
            target: String::new(),
            at: Some(at),
        },
        None,
    );
    let resp = client
        .stack(req)
        .await
        .map_err(|s| status(&s))?
        .into_inner();
    Ok((
        asc_rpc::changes::change_infos(&resp.changes).map_err(fatal)?,
        anchor,
    ))
}

/// The `LogMeta` of `head`: the server's `Log` entry for it carries `change` and `meta`.
async fn log_meta(conn: &remote::server::Wire, head: ChangesetId) -> Result<Option<LogMeta>> {
    let mut stub = RepoClient::new(conn.channel.clone())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE);
    let req = conn.request(
        wire::LogRequest {
            repo: repo_ref(conn),
            include: vec![head.to_string()],
            limit: 1,
            ..wire::LogRequest::default()
        },
        None,
    );
    let mut stream = stub.log(req).await.map_err(|s| status(&s))?.into_inner();
    let Some(first) = stream.message().await.map_err(|s| status(&s))? else {
        return Ok(None);
    };
    // The first entry of a one-entry log of `head` is `head`; anything else is ignored.
    for e in first.entries.iter().take(1) {
        let m = LogMeta::try_from(e).map_err(fatal)?;
        if m.id == head {
            return Ok(Some(m));
        }
    }
    Ok(None)
}

// ------------------------------------------------------------------------------ checkout

/// `path` made absolute and checked to lie inside the parent directory of `root` after the
/// nearest existing ancestor of each is canonicalised (so a symlink cannot lead out).
fn confine_worktree(cwd: &Path, root: &Path, path: &Path) -> Result<PathBuf> {
    let abs = wt::absolutize(cwd, path);
    let outside = || {
        CliError::Raw(
            format!(
                "asc: stack get: --worktree '{}' must lie inside the parent directory of this work tree",
                clean(&path.to_string_lossy())
            ),
            128,
        )
    };
    let parent = root.parent().ok_or_else(outside)?;
    let parent = std::fs::canonicalize(parent).map_err(|_| outside())?;
    let mut existing = abs.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while std::fs::symlink_metadata(&existing).is_err() {
        let Some(name) = existing.file_name().map(std::ffi::OsStr::to_owned) else {
            return Err(outside());
        };
        tail.push(name);
        if !existing.pop() {
            return Err(outside());
        }
    }
    let mut real = std::fs::canonicalize(&existing).map_err(|_| outside())?;
    real.extend(tail.iter().rev());
    if wt::is_inside(&real, &parent) {
        Ok(abs)
    } else {
        Err(outside())
    }
}

/// Checks `commit` out detached: in a new plain worktree at `dest`, else here.
async fn checkout(
    ctx: &Ctx,
    ws: &WsCtx,
    commit: ChangesetId,
    dest: Option<(PathBuf, String)>,
    typed: &str,
) -> Result<Option<PathBuf>> {
    match dest {
        Some((path, as_typed)) => {
            let git = crate::cli::conflict::git_id(ws, commit).await?.to_string();
            let added = worktree::add(
                ctx,
                AddOpts {
                    typed: as_typed,
                    path,
                    branch: None,
                    detach: true,
                    force: false,
                    lock: None,
                    quiet: true,
                    start: Some(git),
                    track: TrackOpt::Auto,
                    ephemeral_secs: None,
                    virtual_mode: false,
                },
            )
            .await?;
            if added.hook_exit != 0 {
                return Err(CliError::Raw(
                    "asc: stack get: the post-checkout hook failed".into(),
                    added.hook_exit,
                ));
            }
            Ok(Some(added.path))
        }
        None => {
            let opts = Opts {
                quiet: true,
                detach: true,
                ..Opts::default()
            };
            perform(
                ctx,
                Move {
                    dest: Dest::Detach { commit },
                    opts,
                    typed: typed.to_owned(),
                    keep_tree: false,
                },
            )
            .await?;
            Ok(None)
        }
    }
}

// ------------------------------------------------------------------------------ get

#[derive(Serialize)]
struct GotChange {
    change: String,
    commit: String,
    frozen: bool,
}

#[derive(Serialize)]
struct Got {
    version: u32,
    changes: Vec<GotChange>,
    /// The change (or snapshot) checked out.
    checked_out: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    worktree: Option<String>,
}

async fn get(ctx: &Ctx, a: GetArgs) -> Result<ExitCode> {
    let target = parse_target(&a.target)?;
    let ws = ctx.ws()?;
    // The destination is judged before anything is fetched.
    let dest = match &a.worktree {
        Some(p) => {
            let root = PathBuf::from(&ws.workspace.ws.path);
            Some((
                confine_worktree(&ctx.cwd, &root, p)?,
                p.to_string_lossy().into_owned(),
            ))
        }
        None => None,
    };
    let name = default_remote(ctx, ws)?;
    let opened = remote::open(ctx, ws, &name, Intent::Read).await?;
    let Some(server) = opened.server.clone() else {
        return Err(CliError::Raw(
            "asc: stack get needs an asc server (not available on a plain git remote)".into(),
            128,
        ));
    };
    let boundaries = StateBoundaries::new(ws);
    let opts = remote::options(ctx, &boundaries);
    let rows = crate::stack::change_rows(ctx)?;

    let (commit, shown, got) = if let Target::Snapshot(id) = &target {
        let src = format!("refs/snapshots/{id}");
        let dst = format!("refs/asc/snapshots/{id}");
        let spec = Refspec::parse(&format!("+{src}:{dst}")).map_err(sync_error)?;
        let mut report = ws
            .engine
            .fetch(&*opened.remote, &[spec], &opts)
            .await
            .map_err(sync_error)?;
        apply_refs(
            ctx,
            std::mem::take(&mut report.local_updates.updates),
            "stack get",
        )
        .await?;
        let Some(RefTarget::Changeset(cs)) = ref_target(ws, &dst).await? else {
            return Err(CliError::Raw(
                format!("asc: stack get: there is no snapshot asc:{id}"),
                1,
            ));
        };
        (cs, format!("asc:{id}"), Vec::new())
    } else {
        let conn = server.session().wire.clone();
        let (infos, anchor) = ask_stack(&conn, &target).await?;
        let picks = select(&infos, anchor, a.downstack, a.upstack)?;
        if picks.is_empty() {
            return Err(CliError::Raw(
                "asc: stack get: the stack has no change to get".into(),
                1,
            ));
        }
        let mut extra = Vec::new();
        for p in &picks {
            extra.push(RefEntry {
                name: ref_name(&format!("refs/asc/stack/{}", p.change))?,
                target: RefTarget::Changeset(p.head),
                version: 0,
                expires_at: None,
            });
        }
        let overlay = Overlay {
            inner: &*opened.remote,
            extra,
        };
        let spec = Refspec::parse("+refs/asc/stack/*:refs/asc/stack/*").map_err(sync_error)?;
        let mut report = ws
            .engine
            .fetch(&overlay, &[spec], &opts)
            .await
            .map_err(sync_error)?;
        apply_refs(
            ctx,
            std::mem::take(&mut report.local_updates.updates),
            "stack get",
        )
        .await?;

        // The metadata of every head, verified against its log entry, then against the claim.
        let ext = ChangesExt::with_advertised(rows.clone(), true);
        let mut metas = Vec::new();
        for p in &picks {
            ws.engine.get_changeset(p.head).await.map_err(fatal)?;
            if let Some(m) = log_meta(&conn, p.head).await? {
                metas.push(m);
            }
        }
        ext.note(&metas).map_err(fatal)?;
        let fetched: HashSet<ChangesetId> = picks.iter().map(|p| p.head).collect();
        ext.apply(&*opened.remote, &fetched).await.map_err(fatal)?;
        verify_rows(&rows, &picks).await?;

        let frozen_now = !a.unfrozen;
        if frozen_now {
            let ids: Vec<ChangeId> = picks.iter().map(|p| p.change).collect();
            rows.freeze(&ids).await.map_err(fatal)?;
        }
        let at = anchor
            .and_then(|c| picks.iter().find(|p| p.change == c))
            .or_else(|| picks.last())
            .ok_or_else(|| fatal("stack get: nothing to check out"))?;
        let mut out = Vec::new();
        for p in &picks {
            out.push(GotChange {
                change: p.change.to_string(),
                commit: crate::cli::conflict::git_id(ws, p.head).await?.to_string(),
                frozen: rows.is_frozen(p.change).await.map_err(fatal)?,
            });
        }
        (at.head, at.change.to_string(), out)
    };

    let made = checkout(ctx, ws, commit, dest.clone(), &shown).await?;
    let mut o = Output::open(ctx, "stack", PagerWant::Never)?;
    if a.json {
        let text = line(&Got {
            version: VERSION,
            changes: got,
            checked_out: shown,
            worktree: made.as_ref().map(|p| p.to_string_lossy().into_owned()),
        })
        .map_err(fatal)?;
        o.write_all(text.as_bytes())?;
    } else {
        for c in &got {
            let tag = if c.frozen { " (frozen)" } else { "" };
            writeln!(o, "{}{tag}", c.change)?;
        }
        if let Some(p) = &made {
            writeln!(o, "worktree {}", clean(&p.to_string_lossy()))?;
        }
    }
    o.finish()?;
    Ok(ExitCode::SUCCESS)
}

sub!(STACK_GET, "get", GetArgs, get);

// ------------------------------------------------------------------------------ freeze

#[derive(Serialize)]
struct Frozen {
    version: u32,
    change: String,
    frozen: bool,
    /// False when the change was already in the requested state.
    changed: bool,
}

async fn set_frozen(ctx: &Ctx, a: FreezeArgs, freeze: bool) -> Result<ExitCode> {
    let l = Local::open(ctx).await?;
    let st = l.stacks().await?;
    let (_, row) = resolve_change(ctx, &l, &st, a.change.as_deref()).await?;
    let changed = if freeze {
        l.rows.freeze(&[row.change]).await.map_err(fatal)? > 0
    } else {
        l.rows.unfreeze(row.change).await.map_err(fatal)?
    };
    let mut o = Output::open(ctx, "stack", PagerWant::Never)?;
    if a.json {
        let text = line(&Frozen {
            version: VERSION,
            change: row.change.to_string(),
            frozen: freeze,
            changed,
        })
        .map_err(fatal)?;
        o.write_all(text.as_bytes())?;
    } else {
        let verb = if freeze { "Froze" } else { "Unfroze" };
        writeln!(o, "{verb} {}", row.change.short())?;
    }
    o.finish()?;
    Ok(ExitCode::SUCCESS)
}

async fn freeze(ctx: &Ctx, a: FreezeArgs) -> Result<ExitCode> {
    set_frozen(ctx, a, true).await
}

async fn unfreeze(ctx: &Ctx, a: FreezeArgs) -> Result<ExitCode> {
    set_frozen(ctx, a, false).await
}

sub!(STACK_FREEZE, "freeze", FreezeArgs, freeze);
sub!(STACK_UNFREEZE, "unfreeze", FreezeArgs, unfreeze);

#[cfg(test)]
mod tests {
    use super::*;

    fn cid(n: u8) -> ChangeId {
        ChangeId::from_bytes([n; 16])
    }

    fn info(n: u8, parent: Option<u8>, landed: bool) -> ChangeInfo {
        ChangeInfo {
            change: cid(n),
            head: ModeId::Asc(ChangesetId::of(&[n])),
            git: None,
            version: 1,
            parent_change: parent.map(cid),
            parent_current: true,
            landed_as: landed.then(|| ModeId::Asc(ChangesetId::of(&[200]))),
            review: None,
            visible: Vec::new(),
        }
    }

    fn changes(p: &[Pick]) -> Vec<ChangeId> {
        p.iter().map(|p| p.change).collect()
    }

    #[test]
    fn targets_are_told_apart() {
        let c = cid(3).to_string();
        assert_eq!(parse_target("#12").unwrap(), Target::Review(12));
        assert_eq!(parse_target("12").unwrap(), Target::Review(12));
        assert_eq!(parse_target(&c).unwrap(), Target::Change(cid(3)));
        assert_eq!(
            parse_target("feature/x").unwrap(),
            Target::Rev("feature/x".into())
        );
        let id = "ab".repeat(20);
        assert_eq!(
            parse_target(&format!("asc:{id}")).unwrap(),
            Target::Snapshot(id)
        );
        for bad in [
            "",
            "asc:xyz",
            "asc:abcd",
            "-x",
            "a\u{1b}b",
            &"x".repeat(MAX_TARGET + 1),
            "99999999999999999999999",
        ] {
            assert!(parse_target(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn selection_follows_the_flags() {
        // 1 <- 2 <- 3, and 2 <- 4 (a fork).
        let all = [
            info(1, None, false),
            info(2, Some(1), false),
            info(3, Some(2), false),
            info(4, Some(2), false),
        ];
        let anchor = Some(cid(2));
        assert_eq!(
            changes(&select(&all, anchor, false, false).unwrap()),
            [cid(1), cid(2), cid(3), cid(4)]
        );
        assert_eq!(
            changes(&select(&all, anchor, true, false).unwrap()),
            [cid(1), cid(2)]
        );
        assert_eq!(
            changes(&select(&all, anchor, false, true).unwrap()),
            [cid(2), cid(3), cid(4)]
        );
        assert_eq!(
            changes(&select(&all, anchor, true, true).unwrap()),
            [cid(1), cid(2), cid(3), cid(4)]
        );
    }

    #[test]
    fn landed_changes_are_never_taken_and_a_landed_target_is_refused() {
        let all = [info(1, None, true), info(2, Some(1), false)];
        assert_eq!(
            changes(&select(&all, Some(cid(2)), false, false).unwrap()),
            [cid(2)]
        );
        assert!(select(&all, Some(cid(1)), false, false).is_err());
    }

    #[test]
    fn a_hostile_answer_is_refused() {
        let twice = [info(1, None, false), info(1, None, false)];
        assert!(select(&twice, None, false, false).is_err());
        let many: Vec<ChangeInfo> = (0..=MAX_CHANGES)
            .map(|i| info(u8::try_from(i % 250).unwrap(), None, false))
            .collect();
        assert!(select(&many, None, false, false).is_err());
        let git = ChangeInfo {
            head: ModeId::Git(asc_core::id::GitOid::from_bytes([1; 20])),
            ..info(1, None, false)
        };
        assert!(select(&[git], None, false, false).is_err());
        assert!(select(&[info(1, None, false)], Some(cid(9)), false, false).is_err());
    }

    #[test]
    fn the_worktree_path_stays_under_the_parent_of_the_work_tree() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let root = base.join("repo");
        std::fs::create_dir(&root).unwrap();
        let ok = confine_worktree(&base, &root, Path::new("sibling")).unwrap();
        assert_eq!(ok, base.join("sibling"));
        assert!(confine_worktree(&base, &root, Path::new("repo/inside")).is_ok());
        for bad in ["../escape", "../../x", "/definitely/elsewhere"] {
            assert!(
                confine_worktree(&root, &root, Path::new(bad)).is_err(),
                "{bad}"
            );
        }
    }
}
