//! `asc evolog [<change>] [-p]` and `asc interdiff <change> [<v1> <v2>]` (SPEC §20.7): the
//! versions of a change and the diff between two of them.
//!
//! A *version* is a changeset the change has had. Locally the versions are the rows of the
//! `changes` index that carry the change id, ordered oldest first by their predecessor links
//! (ties by committer time, then changeset id), and numbered from 1. `interdiff` renders
//! `Engine::interdiff` (PLAN C20) with the ordinary diff output forms.
//!
//! `published` comes from the server's `Changes.Versions` of the change, asked when the default
//! remote is an asc server. Offline, on a plain git remote, with no remote or on any failure the
//! answer is `false` for every version and nothing is printed: the local log is the fallback.
//! `Changes.InterDiff` is not asked; `interdiff` is computed locally.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use bytes::Bytes;
use asc_core::change::ChangeId;
use asc_core::id::ChangesetId;
use asc_engine::change::{ChangeRow, ChangeRows, MAX_ROWS};
use asc_engine::diff::SideKind;
use asc_engine::interdiff::Side;
use asc_rpc::ModeId;
use asc_rpc::server::wire;
use asc_rpc::services::changes_client::ChangesClient;

use super::sl::{Local, clean, fatal};
use crate::cli::common::{CliError, Ctx, Result};
use crate::cli::diff::opts::{Common, apply_config, parse};
use crate::cli::diff::sides::Built;
use crate::cli::diff::work::LocalFiles;
use crate::cli::diff::{Emit, finish, pathspec};
use crate::cli::fetch::default_remote;
use crate::cli::output::Output;
use crate::cli::pager::PagerWant;
use crate::native::NativeSpec;
use crate::remote::{self, Intent};
use crate::stack::json::{Evolog, VersionRow, line};

#[derive(clap::Parser, Debug)]
#[command(name = "evolog", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "evolog",
    flags: &["-p", "--patch", "--json"],
    placeholders: &[],
}];

super::verb!(EVOLOG, "evolog");

mod inter {
    use std::process::ExitCode;

    use super::{Args, interdiff};
    use crate::cli::common::{Ctx, Result};
    use crate::cli::diff::flags;
    use crate::native::NativeSpec;

    pub const NATIVE: &[NativeSpec] = &[NativeSpec {
        cmd: "interdiff",
        flags: flags!(),
        placeholders: &[],
    }];

    async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
        interdiff(ctx, a).await
    }

    crate::stack::cli::verb!(INTERDIFF, "interdiff");
}

/// One local version of a change.
struct Version {
    cs: ChangesetId,
    row: ChangeRow,
    preds: Vec<ChangesetId>,
    time: i64,
}

/// The versions of `change`, oldest first (numbered from 1 by position).
async fn versions(l: &Local<'_>, change: ChangeId) -> Result<Vec<Version>> {
    let rows = l.rows.by_change(change, MAX_ROWS).await.map_err(fatal)?;
    let mut all = Vec::with_capacity(rows.len());
    for row in rows {
        let preds = l.rows.preds(row.cs).await.map_err(fatal)?;
        let c = l.ws.engine.get_changeset(row.cs).await.map_err(fatal)?;
        all.push(Version {
            cs: row.cs,
            row,
            preds,
            time: c.committer.time(),
        });
    }
    // Kahn's algorithm over the predecessor links inside the set; ready ones go by time, id.
    let members: HashSet<ChangesetId> = all.iter().map(|v| v.cs).collect();
    let mut left: HashMap<ChangesetId, usize> = all
        .iter()
        .map(|v| (v.cs, v.preds.iter().filter(|p| members.contains(p)).count()))
        .collect();
    let mut done = vec![false; all.len()];
    let mut order = Vec::with_capacity(all.len());
    while order.len() < all.len() {
        let next = (0..all.len())
            .filter(|&i| !done[i] && left.get(&all[i].cs) == Some(&0))
            .min_by_key(|&i| (all[i].time, all[i].cs.as_bytes().to_vec()));
        let Some(i) = next else {
            return Err(fatal("the predecessor links of a change form a cycle"));
        };
        done[i] = true;
        order.push(i);
        for v in &all {
            if v.preds.contains(&all[i].cs)
                && let Some(n) = left.get_mut(&v.cs)
            {
                *n = n.saturating_sub(1);
            }
        }
    }
    let mut slots: Vec<Option<Version>> = all.into_iter().map(Some).collect();
    Ok(order.into_iter().filter_map(|i| slots[i].take()).collect())
}

/// The server may be slow or gone; evolog never waits longer than this for it.
const SERVER_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);
/// Largest message accepted from the server.
const MAX_MESSAGE: usize = 8 << 20;
/// Pages of `Versions` read, and versions kept.
const MAX_PAGES: usize = 64;

/// The versions the server has recorded for `change`: changeset ids and git ids as hex.
#[derive(Default)]
struct Published {
    cs: HashSet<ChangesetId>,
    git: HashSet<String>,
}

/// Asks `Changes.Versions` of the default remote. `None` when there is no asc server to ask or
/// the ask fails for any reason (offline fallback).
async fn published(ctx: &Ctx, l: &Local<'_>, change: ChangeId) -> Option<Published> {
    let name = default_remote(ctx, l.ws).ok()?;
    ctx.config.get(&format!("remote.{name}.url"))?;
    let ask = async {
        let opened = remote::open(ctx, l.ws, &name, Intent::Read).await.ok()?;
        let server = opened.server.clone()?;
        let wire_conn = server.session().wire.clone();
        let mut client = ChangesClient::new(wire_conn.channel.clone())
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE);
        let mut out = Published::default();
        let mut token = String::new();
        for _ in 0..MAX_PAGES {
            let req = wire_conn.request(
                wire::VersionsRequest {
                    repo: Some(wire::RepoRef {
                        name: wire_conn.repo.clone(),
                        id_mode: asc_rpc::IdMode::Asc.to_wire(),
                    }),
                    change: Bytes::copy_from_slice(change.as_bytes()),
                    page_token: token,
                },
                None,
            );
            let page = client.versions(req).await.ok()?.into_inner();
            for v in &page.versions {
                if let Ok(c) = ModeId::<ChangesetId>::from_wire(&v.cs) {
                    match c {
                        ModeId::Asc(cs) => {
                            out.cs.insert(cs);
                        }
                        ModeId::Git(g) => {
                            out.git.insert(g.to_string());
                        }
                    }
                }
                if !v.git.is_empty() {
                    out.git
                        .insert(v.git.iter().map(|b| format!("{b:02x}")).collect());
                }
            }
            if page.next_page_token.is_empty() {
                return Some(out);
            }
            token = page.next_page_token;
        }
        Some(out)
    };
    tokio::time::timeout(SERVER_BUDGET, ask)
        .await
        .ok()
        .flatten()
}

fn actor_of(name: &[u8], email: &[u8]) -> String {
    format!(
        "{} <{}>",
        clean(&String::from_utf8_lossy(name)),
        clean(&String::from_utf8_lossy(email))
    )
}

fn emit(ctx: &Ctx, text: &str) -> Result<()> {
    let mut out = Output::open(ctx, "evolog", PagerWant::Never)?;
    out.write_all(text.as_bytes())?;
    out.finish()?;
    Ok(())
}

fn common(ctx: &Ctx, argv: &[OsString]) -> Result<Common> {
    let mut seed = asc_engine::diff::DiffOptions::default();
    apply_config(ctx, &mut seed, true)?;
    parse(seed, argv)
}

async fn side(l: &Local<'_>, cs: ChangesetId) -> Result<Side> {
    l.ws.engine.version_side(cs).await.map_err(fatal)
}

/// Prints `changes` with the output forms in `c`.
async fn print(
    ctx: &Ctx,
    l: &Local<'_>,
    c: &mut Common,
    changes: Vec<asc_engine::diff::TreeChange>,
    pager: PagerWant,
    header: Option<&[u8]>,
) -> Result<ExitCode> {
    c.opts.sides = (SideKind::Commit, SideKind::Commit);
    let src = LocalFiles::new(&l.ws.engine);
    let fmt = c.fmt.resolve(false);
    finish(
        ctx,
        c,
        fmt,
        &src,
        Built {
            changes,
            unmerged: Vec::new(),
        },
        Emit {
            pager,
            header,
            plumbing: false,
            attrs: true,
        },
    )
    .await
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let mut json = false;
    let mut patch = false;
    let mut word: Option<String> = None;
    for arg in &a.rest {
        match arg.to_str() {
            Some("--json") => json = true,
            Some("-p" | "--patch") => patch = true,
            Some(w) if !w.starts_with('-') && word.is_none() => word = Some(w.to_owned()),
            _ => {
                return Err(CliError::Usage(
                    "usage: asc evolog [<change>] [-p] [--json]".into(),
                ));
            }
        }
    }
    let l = Local::open(ctx).await?;
    let st = l.stacks().await?;
    let (_, row) = super::info::resolve_change(ctx, &l, &st, word.as_deref()).await?;
    let vs = versions(&l, row.change).await?;
    let known = published(ctx, &l, row.change).await;
    let mut rows = Vec::with_capacity(vs.len());
    for (i, v) in vs.iter().enumerate() {
        let c = l.ws.engine.get_changeset(v.cs).await.map_err(fatal)?;
        let mut preds = Vec::with_capacity(v.preds.len());
        for p in &v.preds {
            preds.push(l.git_id(*p).await?);
        }
        let commit = l.git_id(v.cs).await?;
        let is_published = known
            .as_ref()
            .is_some_and(|k| k.cs.contains(&v.cs) || k.git.contains(&commit));
        rows.push(VersionRow {
            version: u32::try_from(i + 1).map_err(|_| fatal("too many versions"))?,
            commit,
            op: format!("{:?}", v.row.op).to_lowercase(),
            at: c.committer.time(),
            actor: actor_of(c.committer.name(), c.committer.email()),
            preds,
            published: is_published,
        });
    }
    if json {
        let text = line(&Evolog::new(rows)).map_err(fatal)?;
        emit(ctx, &text)?;
        return Ok(ExitCode::SUCCESS);
    }
    let mut text = String::new();
    // Newest first, as a log.
    for r in rows.iter().rev() {
        let short: String = r.commit.chars().take(12).collect();
        text.push_str(&format!(
            "v{}  {}  {}  {}  {}\n",
            r.version, short, r.op, r.at, r.actor
        ));
    }
    emit(ctx, &text)?;
    if patch {
        let mut c = common(ctx, &[])?;
        c.fmt.patch = true;
        for (i, v) in vs.iter().enumerate() {
            let head = format!("\nv{} {}\n", i + 1, rows[i].commit);
            let to = side(&l, v.cs).await?;
            let from = match i {
                0 => Side {
                    root: to.base,
                    base: to.base,
                },
                _ => side(&l, vs[i - 1].cs).await?,
            };
            let changes =
                l.ws.engine
                    .interdiff(from, to, None, &c.opts)
                    .await
                    .map_err(fatal)?;
            print(
                ctx,
                &l,
                &mut c,
                changes,
                PagerWant::Never,
                Some(head.as_bytes()),
            )
            .await?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn version_number(word: &str) -> Result<usize> {
    word.strip_prefix('v')
        .unwrap_or(word)
        .parse::<usize>()
        .ok()
        .filter(|n| *n >= 1)
        .ok_or_else(|| CliError::Fatal(format!("asc: bad version '{}'", clean(word))))
}

async fn interdiff(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let mut c = common(ctx, &a.rest)?;
    let pos: Vec<String> = c
        .positional
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let (word, pair) = match pos.as_slice() {
        [w] => (w.as_str(), None),
        [w, x, y] => (w.as_str(), Some((version_number(x)?, version_number(y)?))),
        _ => {
            return Err(CliError::Usage(
                "usage: asc interdiff <change> [<v1> <v2>]".into(),
            ));
        }
    };
    let l = Local::open(ctx).await?;
    let st = l.stacks().await?;
    let (_, row) = super::info::resolve_change(ctx, &l, &st, Some(word)).await?;
    let vs = versions(&l, row.change).await?;
    let (from, to) = match pair {
        Some(p) => p,
        None if vs.len() >= 2 => (vs.len() - 1, vs.len()),
        None => {
            return Err(CliError::Fatal(
                "asc: the change has only one version".into(),
            ));
        }
    };
    let pick = |n: usize| {
        vs.get(n - 1).map(|v| v.cs).ok_or_else(|| {
            CliError::Fatal(format!("asc: the change has {} versions, not {n}", vs.len()))
        })
    };
    let (f, t) = (pick(from)?, pick(to)?);
    let spec = pathspec(ctx, &c.paths)?;
    let changes =
        l.ws.engine
            .interdiff(
                side(&l, f).await?,
                side(&l, t).await?,
                spec.as_ref(),
                &c.opts,
            )
            .await
            .map_err(fatal)?;
    print(ctx, &l, &mut c, changes, PagerWant::Auto, None).await
}
