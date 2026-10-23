//! `push [<remote> [<refspec>…]]` for bucket remotes (SPEC §6.3, §11.2).
//!
//! Output and rejections are git's, byte for byte: `To <url>`, one aligned line per ref,
//! `! [rejected] … (fetch first|non-fast-forward|stale info|already exists)`, then git's
//! `error: failed to push some refs` and advice. `--porcelain` prints the machine form on
//! stdout. `pre-push` runs before any upload with git SHA-1s on stdin.
//!
//! The decision for each ref is made here from the remote's current value: the closure upload,
//! the compare-and-set of the ref and the demotion are the engine's. Remote-tracking refs move
//! through `Ctx::update_local_refs` from the engine's `local_updates`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::process::ExitCode;

use asc_core::id::{GitOid, RefTarget};
use asc_core::path::RefName;
use asc_engine::history::HistoryApi;
use asc_engine::sync::{Error as SyncError, PushMode, PushUpdate};
use asc_store::refs::{Expect, RefError, RefUpdate, Refs};

use super::common::{CliError, Ctx, Result, WsCtx};
use super::fetch::{Line, current_branch, fatal, git_short, render, sync_error};
use super::remote::apply_refs;
use super::switch::{abbrev_len, ref_name, ref_target, resolve_rev, set_config};
use crate::hooks_runner::{Gate, Hook, HookEnv, PushLine, Runner, null_oid};
use crate::native::NativeSpec;
use crate::remote::{self, Intent, Opened, StateBoundaries};

#[derive(clap::Parser, Debug)]
#[command(name = "push", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "push",
    flags: &[
        "-q",
        "--quiet",
        "-v",
        "--verbose",
        "-u",
        "--set-upstream",
        "-f",
        "--force",
        "--force-with-lease",
        "-d",
        "--delete",
        "--tags",
        "--all",
        "-o",
        "--push-option",
        "-n",
        "--dry-run",
        "--porcelain",
        "--no-verify",
        "--verify",
        "--land",
        "--no-ref",
        "--progress",
        "--no-progress",
        "--no-recurse-submodules",
        "--atomic",
        "--mirror",
        "--force-if-includes",
        "--no-force-with-lease",
        "--end-of-options",
    ],
    placeholders: &[],
}];

const USAGE: &str = "usage: git push [<options>] [<repository> [<refspec>...]]";

#[derive(Clone, Debug, Default)]
struct Opts {
    quiet: bool,
    verbose: bool,
    force: bool,
    /// `Some(None)`: bare `--force-with-lease`; `Some(Some(spec))`: `<ref>[:<expect>]`.
    lease: Option<Option<String>>,
    delete: bool,
    tags: bool,
    all: bool,
    set_upstream: bool,
    dry_run: bool,
    porcelain: bool,
    no_verify: bool,
    land: bool,
    no_ref: bool,
    push_options: Vec<String>,
    words: Vec<String>,
}

fn parse(argv: &[OsString]) -> Result<Opts> {
    let mut o = Opts::default();
    let mut it = argv.iter().map(|a| a.to_string_lossy().into_owned());
    let mut ended = false;
    while let Some(a) = it.next() {
        if ended || !a.starts_with('-') || a == "-" {
            o.words.push(a);
            continue;
        }
        match a.as_str() {
            "--" | "--end-of-options" => ended = true,
            "-q" | "--quiet" => o.quiet = true,
            "-v" | "--verbose" => o.verbose = true,
            "-u" | "--set-upstream" => o.set_upstream = true,
            "-f" | "--force" => o.force = true,
            "--force-with-lease" => o.lease = Some(None),
            "--no-force-with-lease" => o.lease = None,
            "-d" | "--delete" => o.delete = true,
            "--tags" => o.tags = true,
            "--all" => o.all = true,
            "-n" | "--dry-run" => o.dry_run = true,
            "--porcelain" => o.porcelain = true,
            "--no-verify" => o.no_verify = true,
            "--verify" => o.no_verify = false,
            "--land" => o.land = true,
            "--no-ref" => o.no_ref = true,
            "-o" | "--push-option" => o
                .push_options
                .push(it.next().ok_or_else(|| CliError::Usage(USAGE.to_owned()))?),
            "--progress"
            | "--no-progress"
            | "--no-recurse-submodules"
            | "--atomic"
            | "--force-if-includes" => {}
            "--mirror" => return Err(CliError::Fatal("asc: --mirror is not supported".into())),
            w if w.starts_with("--force-with-lease=") => {
                o.lease = Some(Some(w["--force-with-lease=".len()..].to_owned()));
            }
            w if w.starts_with("--push-option=") => {
                o.push_options.push(w["--push-option=".len()..].to_owned())
            }
            _ => return Err(CliError::Usage(USAGE.to_owned())),
        }
    }
    Ok(o)
}

/// One requested update: `+src:dst`.
#[derive(Clone, Debug)]
struct Spec {
    /// The local side as typed (`None`: delete).
    src: Option<String>,
    dst: String,
    force: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    New,
    FastForward,
    Forced,
    Delete,
    UpToDate,
    Rejected(&'static str),
}

struct Plan {
    spec: Spec,
    /// The full local ref (or `HEAD`) the new value came from; empty for a delete.
    src_label: String,
    new: Option<RefTarget>,
    old: Option<RefTarget>,
    kind: Kind,
}

fn full_heads(name: &str) -> String {
    if name.starts_with("refs/") {
        name.to_owned()
    } else {
        format!("refs/heads/{name}")
    }
}

fn short(full: &str) -> String {
    full.strip_prefix("refs/heads/")
        .or_else(|| full.strip_prefix("refs/tags/"))
        .unwrap_or(full)
        .to_owned()
}

/// The remote a bare `push` uses.
fn push_remote(ctx: &Ctx, ws: &WsCtx) -> Result<String> {
    if let Some(b) = current_branch(ws)? {
        if let Some(r) = ctx.config.get(&format!("branch.{b}.pushRemote")) {
            return Ok(r);
        }
        if let Some(r) = ctx.config.get("remote.pushDefault") {
            return Ok(r);
        }
        if let Some(r) = ctx.config.get(&format!("branch.{b}.remote"))
            && r != "."
        {
            return Ok(r);
        }
    } else if let Some(r) = ctx.config.get("remote.pushDefault") {
        return Ok(r);
    }
    Ok("origin".to_owned())
}

fn failed_to_push(url: &str) -> String {
    format!("error: failed to push some refs to '{url}'")
}

/// Expands the refspecs on the command line, or the default for a bare `push`.
async fn specs(
    ctx: &Ctx,
    ws: &WsCtx,
    o: &Opts,
    remote_name: &str,
    rest: &[String],
) -> Result<(Vec<Spec>, Option<(String, String)>)> {
    let prog = ctx.prog.name();
    let mut out = Vec::new();
    let mut upstream: Option<(String, String)> = None;
    if o.all {
        if !rest.is_empty() || o.delete {
            return Err(CliError::Fatal(
                "--all can't be combined with refspecs".into(),
            ));
        }
        for e in ws.db.refs().list(ws.repo.key, "refs/heads/").await? {
            let name = String::from_utf8_lossy(e.name.as_bytes()).into_owned();
            out.push(Spec {
                src: Some(name.clone()),
                dst: name,
                force: false,
            });
        }
    }
    if o.delete {
        if rest.is_empty() {
            return Err(CliError::Fatal(
                "--delete doesn't make sense without any refs".into(),
            ));
        }
        for w in rest {
            out.push(Spec {
                src: None,
                dst: full_heads(w),
                force: false,
            });
        }
    } else {
        for w in rest {
            let (force, body) = w
                .strip_prefix('+')
                .map_or((false, w.as_str()), |b| (true, b));
            let (src, dst) = match body.split_once(':') {
                Some((s, d)) => (s, Some(d)),
                None => (body, None),
            };
            if src.is_empty() {
                let Some(d) = dst.filter(|d| !d.is_empty()) else {
                    return Err(CliError::Fatal(format!("invalid refspec '{w}'")));
                };
                out.push(Spec {
                    src: None,
                    dst: full_heads(d),
                    force,
                });
                continue;
            }
            out.push(Spec {
                src: Some(src.to_owned()),
                dst: dst.unwrap_or("").to_owned(),
                force,
            });
        }
    }
    if o.tags {
        for e in ws.db.refs().list(ws.repo.key, "refs/tags/").await? {
            let name = String::from_utf8_lossy(e.name.as_bytes()).into_owned();
            out.push(Spec {
                src: Some(name.clone()),
                dst: name,
                force: false,
            });
        }
    }
    if out.is_empty() && !o.no_ref {
        let Some(branch) = current_branch(ws)? else {
            return Err(CliError::Raw(
                format!(
                    "fatal: You are not currently on a branch.\nTo push the history leading to the current (detached HEAD)\nstate now, use\n\n    {prog} push {remote_name} HEAD:<name-of-remote-branch>\n"
                ),
                128,
            ));
        };
        let mode = ctx
            .config
            .get("push.default")
            .unwrap_or_else(|| "simple".to_owned());
        let merge = ctx.config.get(&format!("branch.{branch}.merge"));
        let configured_remote = ctx.config.get(&format!("branch.{branch}.remote"));
        let has_upstream = merge.is_some() && configured_remote.as_deref() == Some(remote_name);
        let auto = ctx.config.bool("push.autoSetupRemote").unwrap_or(false);
        let dst = match mode.as_str() {
            "current" => format!("refs/heads/{branch}"),
            "nothing" => {
                return Err(CliError::Fatal(
                    "You didn't specify any refspecs to push, and push.default is \"nothing\"."
                        .into(),
                ));
            }
            "upstream" | "tracking" | "simple" => match (&merge, has_upstream) {
                (Some(m), true) => {
                    if mode == "simple" && short(m) != branch {
                        return Err(CliError::Raw(
                            format!(
                                "fatal: The upstream branch of your current branch does not match\nthe name of your current branch.  To push to the upstream branch\non the remote, use\n\n    {prog} push {remote_name} HEAD:{m}\n\nTo push to the branch of the same name on the remote, use\n\n    {prog} push {remote_name} HEAD\n"
                            ),
                            128,
                        ));
                    }
                    m.clone()
                }
                _ if auto => {
                    upstream = Some((branch.clone(), format!("refs/heads/{branch}")));
                    format!("refs/heads/{branch}")
                }
                _ => {
                    return Err(CliError::Raw(
                        format!(
                            "fatal: The current branch {branch} has no upstream branch.\nTo push the current branch and set the remote as upstream, use\n\n    {prog} push --set-upstream {remote_name} {branch}\n\nTo have this happen automatically for branches without a tracking\nupstream, see 'push.autoSetupRemote' in '{prog} help config'.\n"
                        ),
                        128,
                    ));
                }
            },
            "matching" => {
                for e in ws.db.refs().list(ws.repo.key, "refs/heads/").await? {
                    let name = String::from_utf8_lossy(e.name.as_bytes()).into_owned();
                    out.push(Spec {
                        src: Some(name.clone()),
                        dst: name,
                        force: false,
                    });
                }
                String::new()
            }
            other => {
                return Err(CliError::Fatal(format!(
                    "unknown push.default value: {other}"
                )));
            }
        };
        if !dst.is_empty() {
            out.push(Spec {
                src: Some(format!("refs/heads/{branch}")),
                dst,
                force: false,
            });
        }
    }
    if out.len() > 10_000 {
        return Err(CliError::Fatal(
            "asc: too many refs to push in one command".into(),
        ));
    }
    Ok((out, upstream))
}

/// What a local source names: its full label and value.
async fn resolve_source(ctx: &Ctx, ws: &WsCtx, src: &str) -> Result<Option<(String, RefTarget)>> {
    if src == "HEAD" {
        return Ok(match current_branch(ws)? {
            Some(b) => ref_target(ws, &format!("refs/heads/{b}"))
                .await?
                .map(|t| (format!("refs/heads/{b}"), t)),
            None => ws
                .head_commit()
                .await?
                .map(|c| ("HEAD".to_owned(), RefTarget::Changeset(c))),
        });
    }
    let candidates = if src.starts_with("refs/") {
        vec![src.to_owned()]
    } else {
        vec![format!("refs/heads/{src}"), format!("refs/tags/{src}")]
    };
    for c in candidates {
        if let Some(t) = ref_target(ws, &c).await? {
            return Ok(Some((c, t)));
        }
    }
    Ok(resolve_rev(ctx, ws, src)
        .await?
        .map(|r| (src.to_owned(), RefTarget::Changeset(r.commit))))
}

fn destination(spec: &Spec, label: &str) -> std::result::Result<String, String> {
    if spec.dst.is_empty() {
        return if label.starts_with("refs/") {
            Ok(label.to_owned())
        } else {
            Err(format!(
                "The <src> part of the refspec is a commit object.\nDid you mean to create a new branch by pushing to\n'{label}:refs/heads/<name>'?"
            ))
        };
    }
    if spec.dst.starts_with("refs/") {
        return Ok(spec.dst.clone());
    }
    if label.starts_with("refs/tags/") {
        Ok(format!("refs/tags/{}", spec.dst))
    } else if label.starts_with("refs/heads/") {
        Ok(format!("refs/heads/{}", spec.dst))
    } else {
        Err(format!(
            "The destination you provided is not a full refname (i.e.,\nstarting with \"refs/\"). We tried to guess what you meant by:\n\n- Looking for a ref that matches '{}' on the remote side.\n- Checking if the <src> being pushed ('{label}')\n  is a ref in \"refs/{{heads,tags}}/\". If so we add a corresponding\n  refs/{{heads,tags}}/ prefix on the remote side.\n\nNeither worked, so we gave up. You must fully qualify the ref.",
            spec.dst
        ))
    }
}

/// The `<ref>[:<expect>]` of `--force-with-lease`, resolved to the value the remote must have.
async fn lease_for(
    ctx: &Ctx,
    ws: &WsCtx,
    opened: &Opened,
    lease: &Option<Option<String>>,
    dst: &str,
) -> Result<Option<Option<RefTarget>>> {
    let Some(l) = lease else { return Ok(None) };
    let Some(text) = l else {
        return Ok(Some(tracking_value(ws, opened, dst).await?));
    };
    let (name, expect) = match text.split_once(':') {
        Some((n, e)) => (n, Some(e)),
        None => (text.as_str(), None),
    };
    if full_heads(name) != dst && name != short(dst) {
        return Ok(None);
    }
    match expect {
        Some("") => Ok(Some(None)),
        Some(rev) => {
            let r = resolve_rev(ctx, ws, rev).await?.ok_or_else(|| {
                CliError::Fatal(format!("cannot parse expected object name '{rev}'"))
            })?;
            Ok(Some(Some(RefTarget::Changeset(r.commit))))
        }
        None => Ok(Some(tracking_value(ws, opened, dst).await?)),
    }
}

async fn tracking_value(ws: &WsCtx, opened: &Opened, dst: &str) -> Result<Option<RefTarget>> {
    match dst.strip_prefix("refs/heads/") {
        Some(b) if opened.configured => {
            ref_target(ws, &format!("refs/remotes/{}/{b}", opened.name)).await
        }
        _ => Ok(None),
    }
}

async fn local_commit(ws: &WsCtx, t: RefTarget) -> bool {
    match t {
        RefTarget::Changeset(c) => ws.engine.get_changeset(c).await.is_ok(),
        RefTarget::Tag(_) => true,
    }
}

async fn git_of(ws: &WsCtx, t: Option<RefTarget>) -> GitOid {
    match t {
        Some(RefTarget::Changeset(c)) => super::conflict::git_id(ws, c)
            .await
            .unwrap_or_else(|_| null_oid()),
        Some(RefTarget::Tag(tag)) => {
            asc_state::state::git_map::asc_to_git(&ws.db, ws.repo.key, tag.as_bytes())
                .await
                .ok()
                .and_then(|rows| rows.into_iter().next().map(|(o, _)| o))
                .unwrap_or_else(null_oid)
        }
        None => null_oid(),
    }
}

fn advice(ctx: &Ctx, key: &str) -> bool {
    ctx.config.bool("advice.pushUpdateRejected").unwrap_or(true)
        && ctx.config.bool(&format!("advice.{key}")).unwrap_or(true)
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let o = parse(&a.rest)?;
    let prog = ctx.prog.name();
    if o.land {
        return Err(CliError::Fatal(
            "asc: --land needs an asc server remote".into(),
        ));
    }
    if !o.push_options.is_empty() {
        return Err(CliError::Fatal(
            "the receiving end does not support push options".into(),
        ));
    }
    let (target, rest) = match o.words.split_first() {
        Some((first, rest)) => (first.clone(), rest.to_vec()),
        None => (push_remote(ctx, ws)?, Vec::new()),
    };
    let opened = remote::open(ctx, ws, &target, Intent::Write).await?;
    let shown_url = super::url_display::anonymize(&opened.url);
    let abbrev = abbrev_len(ctx);
    let boundaries = StateBoundaries::new(ws);
    let mut sync_opts = remote::options(ctx, &boundaries);
    let demote = if matches!(ws.repo.repo.remote_url.as_deref(), Some(u) if matches!(&opened.spec, crate::remote::url::RemoteSpec::Bucket { url, .. } if url == u))
    {
        Some(remote::StoreDemote(remote::store_set(ctx, ws)?))
    } else {
        None
    };
    if let Some(d) = &demote {
        sync_opts.demote = Some(d);
    }

    if o.no_ref {
        let rev = rest.first().map_or("HEAD", String::as_str);
        let (_, value) = resolve_source(ctx, ws, rev)
            .await?
            .ok_or_else(|| CliError::Fatal(format!("src refspec {rev} does not match any")))?;
        let dst = ref_name("refs/heads/no-ref")?;
        let upd = [PushUpdate {
            dst,
            new: Some(value),
            expect: Expect::Any,
        }];
        if !o.dry_run {
            remote::claim_local_refs(ws, &opened).await?;
            ws.engine
                .push(&*opened.remote, &upd, PushMode::NoRef, &sync_opts)
                .await
                .map_err(sync_error)?;
        }
        return Ok(ExitCode::SUCCESS);
    }

    let (requested, upstream) = specs(ctx, ws, &o, &opened.name, &rest).await?;
    let remote_refs: HashMap<String, RefTarget> = opened
        .remote
        .list_refs()
        .await
        .map_err(sync_error)?
        .into_iter()
        .map(|r| {
            (
                String::from_utf8_lossy(r.name.as_bytes()).into_owned(),
                r.target,
            )
        })
        .collect();

    let mut plans: Vec<Plan> = Vec::new();
    for spec in requested {
        let (label, new) = match &spec.src {
            None => (String::new(), None),
            Some(src) => match resolve_source(ctx, ws, src).await? {
                Some((label, value)) => (label, Some(value)),
                None => {
                    eprintln!("error: src refspec {src} does not match any");
                    eprintln!("{}", failed_to_push(&shown_url));
                    return Ok(ExitCode::from(1));
                }
            },
        };
        let dst = match destination(&spec, &label) {
            Ok(d) => d,
            Err(text) => {
                eprintln!(
                    "error: {}",
                    text.replace('\n', "\nerror: ").replace("error: \n", "\n")
                );
                eprintln!("{}", failed_to_push(&shown_url));
                return Ok(ExitCode::from(1));
            }
        };
        if RefName::new(dst.clone().into_bytes()).is_err() || dst.starts_with("refs/for/") {
            return Err(CliError::Fatal(format!(
                "asc: cannot push to '{dst}' on a bucket remote"
            )));
        }
        let old = remote_refs.get(&dst).copied();
        let lease = lease_for(ctx, ws, &opened, &o.lease, &dst).await?;
        let force = o.force || spec.force;
        // The remote's tip is known here when it is what the last fetch stored (objects are
        // machine-wide, so their presence says nothing about this repository).
        let tracked = tracking_value(ws, &opened, &dst).await?;
        let known = |t: RefTarget| tracked == Some(t);
        let kind = match (new, old) {
            (None, None) => {
                eprintln!(
                    "error: unable to delete '{}': remote ref does not exist",
                    short(&dst)
                );
                eprintln!("{}", failed_to_push(&shown_url));
                return Ok(ExitCode::from(1));
            }
            (None, Some(_)) => Kind::Delete,
            (Some(n), Some(old)) if n == old => Kind::UpToDate,
            _ if lease.is_some_and(|expected| expected != old) => Kind::Rejected("stale info"),
            (Some(_), None) => Kind::New,
            (Some(_), Some(_)) if force || lease.is_some() => {
                // A lease or --force lets any update through; only the display differs.
                match (new, old) {
                    (Some(RefTarget::Changeset(n)), Some(RefTarget::Changeset(old)))
                        if (known(RefTarget::Changeset(old))
                            || (!opened.configured
                                && local_commit(ws, RefTarget::Changeset(old)).await))
                            && HistoryApi::is_ancestor(&ws.engine, old, n)
                                .await
                                .unwrap_or(false) =>
                    {
                        Kind::FastForward
                    }
                    _ => Kind::Forced,
                }
            }
            (Some(RefTarget::Changeset(n)), Some(RefTarget::Changeset(old))) => {
                if !(known(RefTarget::Changeset(old))
                    || (!opened.configured && local_commit(ws, RefTarget::Changeset(old)).await))
                {
                    Kind::Rejected("fetch first")
                } else if HistoryApi::is_ancestor(&ws.engine, old, n)
                    .await
                    .map_err(fatal)?
                {
                    Kind::FastForward
                } else {
                    Kind::Rejected("non-fast-forward")
                }
            }
            (Some(_), Some(_)) if dst.starts_with("refs/tags/") => Kind::Rejected("already exists"),
            (Some(_), Some(_)) => Kind::Rejected("non-fast-forward"),
        };
        let kind = if dst.starts_with("refs/tags/")
            && matches!(kind, Kind::Forced | Kind::FastForward)
            && !force
        {
            Kind::Rejected("already exists")
        } else {
            kind
        };
        plans.push(Plan {
            spec: Spec { dst, ..spec },
            src_label: label,
            new,
            old,
            kind,
        });
    }

    // pre-push: before any upload, with git ids.
    if !o.no_verify && !o.dry_run {
        let mut lines = Vec::new();
        for p in &plans {
            if matches!(p.kind, Kind::UpToDate | Kind::Rejected(_)) {
                continue;
            }
            lines.push(PushLine {
                local_ref: if p.new.is_none() {
                    "(delete)".to_owned()
                } else {
                    p.src_label.clone()
                },
                local: git_of(ws, p.new).await,
                remote_ref: p.spec.dst.clone(),
                remote: git_of(ws, p.old).await,
            });
        }
        let runner = Runner::for_ctx(ctx)?;
        let hook = Hook::PrePush {
            remote: opened.name.clone(),
            url: opened.url.clone(),
            lines,
        };
        match runner
            .gate(hook, &HookEnv::of(ctx, "push"))
            .await
            .map_err(fatal)?
        {
            Gate::Pass => {}
            Gate::Reject(_) => {
                eprintln!("{}", failed_to_push(&shown_url));
                return Ok(ExitCode::from(1));
            }
        }
    }

    // The upload and the ref update.
    let to_send: Vec<&Plan> = plans
        .iter()
        .filter(|p| !matches!(p.kind, Kind::UpToDate | Kind::Rejected(_)))
        .collect();
    let mut raced = false;
    if !to_send.is_empty() && !o.dry_run {
        remote::claim_local_refs(ws, &opened).await?;
        let updates: Vec<PushUpdate> = to_send
            .iter()
            .map(|p| PushUpdate {
                dst: ref_name(&p.spec.dst).expect("validated above"),
                new: p.new,
                expect: p.old.map_or(Expect::Absent, Expect::Value),
            })
            .collect();
        match ws
            .engine
            .push(
                &*opened.remote,
                &updates,
                PushMode::ForceWithLease,
                &sync_opts,
            )
            .await
        {
            Ok(report) => {
                if opened.configured {
                    let mut tracking: Vec<RefUpdate> = Vec::new();
                    for u in report.local_updates.updates {
                        let exists = ref_target(ws, &String::from_utf8_lossy(u.name.as_bytes()))
                            .await?
                            .is_some();
                        if u.new.is_none() && !exists {
                            continue;
                        }
                        tracking.push(u);
                    }
                    apply_refs(ctx, tracking, "update by push").await?;
                }
            }
            Err(SyncError::Refs(RefError::Conflict { .. })) => raced = true,
            Err(e) => return Err(sync_error(e)),
        }
    }

    // Upstream configuration (`-u`, `push.autoSetupRemote`).
    let mut tracking_notes: Vec<String> = Vec::new();
    if (o.set_upstream || upstream.is_some()) && opened.configured && !o.dry_run && !raced {
        for p in &plans {
            if matches!(p.kind, Kind::Rejected(_)) {
                continue;
            }
            if let Some(branch) = p.src_label.strip_prefix("refs/heads/")
                && let Some(dst_branch) = p.spec.dst.strip_prefix("refs/heads/")
            {
                set_config(ctx, &format!("branch.{branch}.remote"), &opened.name).await?;
                set_config(ctx, &format!("branch.{branch}.merge"), &p.spec.dst).await?;
                tracking_notes.push(format!(
                    "branch '{branch}' set up to track '{}/{dst_branch}'.",
                    opened.name
                ));
            }
        }
    }

    // Report.
    let mut lines: Vec<Line> = Vec::new();
    let mut porcelain = String::new();
    let mut hints: Vec<&'static str> = Vec::new();
    let head_branch = current_branch(ws)?.map(|b| format!("refs/heads/{b}"));
    let mut any_rejected = raced;
    for p in &plans {
        let kind = if raced && !matches!(p.kind, Kind::UpToDate | Kind::Rejected(_)) {
            Kind::Rejected("fetch first")
        } else {
            p.kind.clone()
        };
        let from = if p.new.is_some() {
            short(&p.src_label)
        } else {
            String::new()
        };
        let to = short(&p.spec.dst);
        let (flag, summary, reason): (char, String, Option<String>) = match &kind {
            Kind::New => {
                let word = if p.spec.dst.starts_with("refs/tags/") {
                    "[new tag]"
                } else {
                    "[new branch]"
                };
                ('*', word.to_owned(), None)
            }
            Kind::FastForward => {
                let (x, y) = (
                    git_short(ws, p.old, abbrev).await,
                    git_short(ws, p.new, abbrev).await,
                );
                (' ', format!("{x}..{y}"), None)
            }
            Kind::Forced => {
                let (x, y) = (
                    git_short(ws, p.old, abbrev).await,
                    git_short(ws, p.new, abbrev).await,
                );
                ('+', format!("{x}...{y}"), Some("forced update".to_owned()))
            }
            Kind::Delete => ('-', "[deleted]".to_owned(), None),
            Kind::UpToDate => ('=', "[up to date]".to_owned(), None),
            Kind::Rejected(why) => {
                any_rejected = true;
                let hint = match *why {
                    "fetch first" => Some((
                        "pushFetchFirst",
                        "hint: Updates were rejected because the remote contains work that you do not\nhint: have locally. This is usually caused by another repository pushing to\nhint: the same ref. If you want to integrate the remote changes, use\nhint: 'PROG pull' before pushing again.\nhint: See the 'Note about fast-forwards' in 'PROG push --help' for details.",
                    )),
                    "non-fast-forward" if Some(&p.spec.dst) == head_branch.as_ref() => Some((
                        "pushNonFFCurrent",
                        "hint: Updates were rejected because the tip of your current branch is behind\nhint: its remote counterpart. If you want to integrate the remote changes,\nhint: use 'PROG pull' before pushing again.\nhint: See the 'Note about fast-forwards' in 'PROG push --help' for details.",
                    )),
                    "non-fast-forward" => Some((
                        "pushNonFFMatching",
                        "hint: Updates were rejected because a pushed branch tip is behind its remote\nhint: counterpart. If you want to integrate the remote changes, use 'PROG pull'\nhint: before pushing again.\nhint: See the 'Note about fast-forwards' in 'PROG push --help' for details.",
                    )),
                    "already exists" => Some((
                        "pushAlreadyExists",
                        "hint: Updates were rejected because the tag already exists in the remote.",
                    )),
                    _ => None,
                };
                if let Some((key, text)) = hint
                    && advice(ctx, key)
                    && !hints.contains(&text)
                {
                    hints.push(text);
                }
                ('!', "[rejected]".to_owned(), Some((*why).to_owned()))
            }
        };
        if o.porcelain {
            let pair = format!(
                "{}:{}",
                if p.new.is_some() {
                    p.src_label.clone()
                } else {
                    String::new()
                },
                p.spec.dst
            );
            let tail = reason
                .as_ref()
                .map(|r| {
                    if flag == '!' {
                        format!(" ({r})")
                    } else {
                        String::new()
                    }
                })
                .unwrap_or_default();
            porcelain.push_str(&format!("{flag}\t{pair}\t{summary}{tail}\n"));
        } else if flag == '=' && !o.verbose {
            continue;
        } else if flag == '-' {
            lines.push(Line {
                flag,
                summary,
                from: to,
                to: String::new(),
                reason: None,
            });
        } else {
            lines.push(Line {
                flag,
                summary,
                from,
                to,
                reason,
            });
        }
    }

    if !o.quiet {
        if o.porcelain {
            print!("To {shown_url}\n{porcelain}Done\n");
        } else if lines.is_empty() && !any_rejected {
            eprintln!("Everything up-to-date");
        } else if !lines.is_empty() {
            eprintln!("To {shown_url}");
            eprint!("{}", render(&lines, abbrev, false));
        }
    }
    if any_rejected {
        eprintln!("{}", failed_to_push(&shown_url));
        if !o.quiet || !hints.is_empty() {
            for h in hints {
                eprintln!("{}", h.replace("PROG", prog));
            }
        }
    }
    for note in tracking_notes {
        println!("{note}");
    }
    Ok(if any_rejected {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(src: Option<&str>, dst: &str) -> Spec {
        Spec {
            src: src.map(str::to_owned),
            dst: dst.to_owned(),
            force: false,
        }
    }

    #[test]
    fn destinations_follow_the_source_kind() {
        assert_eq!(
            destination(&spec(Some("a"), ""), "refs/heads/a").unwrap(),
            "refs/heads/a"
        );
        assert_eq!(
            destination(&spec(Some("a"), "b"), "refs/heads/a").unwrap(),
            "refs/heads/b"
        );
        assert_eq!(
            destination(&spec(Some("t"), "u"), "refs/tags/t").unwrap(),
            "refs/tags/u"
        );
        assert_eq!(
            destination(&spec(Some("a"), "refs/heads/x"), "HEAD").unwrap(),
            "refs/heads/x"
        );
        assert!(destination(&spec(Some("abc123"), "b"), "abc123").is_err());
        assert!(destination(&spec(Some("abc123"), ""), "abc123").is_err());
    }

    #[test]
    fn short_names() {
        assert_eq!(short("refs/heads/a/b"), "a/b");
        assert_eq!(short("refs/tags/v1"), "v1");
        assert_eq!(short("refs/other/x"), "refs/other/x");
        assert_eq!(full_heads("x"), "refs/heads/x");
        assert_eq!(full_heads("refs/tags/x"), "refs/tags/x");
    }

    #[test]
    fn options_parse() {
        let argv: Vec<OsString> = ["-u", "--force-with-lease=master:abc", "origin", "master"]
            .iter()
            .map(Into::into)
            .collect();
        let o = parse(&argv).unwrap();
        assert!(o.set_upstream);
        assert_eq!(o.lease, Some(Some("master:abc".to_owned())));
        assert_eq!(o.words, ["origin", "master"]);
        assert!(parse(&[OsString::from("--bogus")]).is_err());
        assert!(parse(&[OsString::from("--mirror")]).is_err());
    }
}
