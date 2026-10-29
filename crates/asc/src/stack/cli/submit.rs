//! `asc submit [<change>] [-s] [--draft|--publish] [--reviewer <u>…] [--update-only]
//! [--auto-merge] [--force]` (SPEC §20.5 flow 1, §20.7): pushes the changes from the bottom of
//! the stack up to `<change>` (default the current change; the whole stack with `-s`) as ONE
//! `UpdateRefs` whose updates are `refs/for/<trunk>/asc-<change>`, bottom to top, so the server
//! creates (or versions) the pull requests in stack order. The server finds a PR by change id,
//! not by topic and user, and decides who may update it.
//!
//! What the client does before it sends anything:
//!
//! * a plain git remote (or a bucket) has no `Changes` service: exit 128 with the Stage 0 text;
//! * a change that needs a restack, or is divergent, stops the run (exit 128), as does a stack
//!   that rests on a merge, and a change the server already shows as landed or merged. The
//!   check is local first, so nothing is uploaded for a stack that cannot be submitted;
//! * reviewer names are checked against a conservative handle alphabet (a subset of the server's
//!   user handles), because they travel in a push option and a comma, `=` or control character
//!   would change what the option says;
//! * `--update-only` needs `Changes.ResolveChanges` and sends only the changes that already have
//!   a PR (the server is told too, with `asc.update_only`); when the server cannot say, nothing is
//!   sent.
//!
//! The `m/` objects of the changes go up with the closure only when the server advertises
//! `changes.v1` (C23, `ChangesExt`); the push options are `asc.draft=<bool>`,
//! `asc.reviewers=<u>,<u>`, `asc.update_only`, `asc.auto_merge` and AGit's `force-push` for
//! `--force`. Everything the server answers (messages, PR links) is printed with control
//! characters replaced, and its counts are bounded.
//!
//! Local freezing arrives with `asc stack freeze` (C26); until it lands there is no local frozen
//! state to refuse on, and the server's `CHANGE_FROZEN` is reported as it is.

use std::collections::{HashMap, HashSet};
use std::process::ExitCode;

use asc_core::change::ChangeId;
use asc_core::id::{ChangesetId, RefTarget};
use asc_engine::sync::changes::ChangesExt;
use asc_engine::sync::{PushMode, PushUpdate};
use asc_rpc::IdMode;
use asc_rpc::changes::{ChangeInfo, change_infos};
use asc_rpc::client::CallError;
use asc_rpc::server::{MAX_MESSAGE, wire};
use asc_rpc::services::changes_client::ChangesClient;
use asc_state::changes::StateChanges;
use asc_store::refs::Expect;
use serde::Serialize;
use tonic::Code;

use super::info::resolve_change;
use super::sl::{LStacks, Local, clean, fatal, stack_err};
use super::uncommit::subject_of;
use crate::cli::common::{CliError, Ctx, Result};
use crate::cli::fetch::sync_error;
use crate::cli::switch::ref_name;
use crate::native::NativeSpec;
use crate::remote::server::{PushIntent, ServerRemote};
use crate::remote::{self, Intent, StateBoundaries};
use crate::stack::json::{VERSION, line};

/// Most changes one submit sends (the server's `Changes` limit per stack and per request).
pub const MAX_CHANGES: usize = 256;
/// Most reviewers one submit names.
pub const MAX_REVIEWERS: usize = 32;
/// Longest reviewer handle this client sends (shorter than the server's own limit).
pub const MAX_REVIEWER_LEN: usize = 40;
/// Most lines of the server's message that are printed.
const MAX_MESSAGE_LINES: usize = 200;
/// Longest printed line of the server's message.
const MAX_MESSAGE_LINE: usize = 1024;

#[derive(clap::Parser, Debug)]
#[command(name = "submit", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Submit up to this change; default the current change.
    pub change: Option<String>,
    /// The whole stack, not only bottom..change.
    #[arg(short = 's', long = "stack")]
    pub stack: bool,
    /// Open the PRs as drafts (the default unless `asc.stack.submit.draft` says otherwise).
    #[arg(long, conflicts_with = "publish")]
    pub draft: bool,
    /// Open the PRs ready for review.
    #[arg(long)]
    pub publish: bool,
    /// Ask this user to review (repeatable, or comma separated).
    #[arg(long = "reviewer", value_delimiter = ',')]
    pub reviewer: Vec<String>,
    /// Only update PRs that exist; create none.
    #[arg(long = "update-only")]
    pub update_only: bool,
    /// Ask the server to merge the PRs when they are ready.
    #[arg(long = "auto-merge")]
    pub auto_merge: bool,
    /// Replace a version the server holds that this one does not supersede.
    #[arg(long)]
    pub force: bool,
    /// Report what would be sent; send nothing.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
    #[arg(long)]
    pub json: bool,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "submit",
    flags: &[
        "-s",
        "--stack",
        "--draft",
        "--publish",
        "--reviewer",
        "--update-only",
        "--auto-merge",
        "--force",
        "-n",
        "--dry-run",
        "--json",
    ],
    placeholders: &[],
}];

super::verb!(SUBMIT, "submit");
// `asc stack submit` is the same command as `asc submit` (SPEC 20.7 lists the stack verbs under both spellings).
super::info::sub!(STACK_SUBMIT, "submit", Args, run);

// ------------------------------------------------------------------------------ pure parts

/// Whether `name` is a reviewer name that cannot change the meaning of the push option it
/// travels in: a handle of `[A-Za-z0-9._-]`, 1..=[`MAX_REVIEWER_LEN`] bytes, not starting with a
/// dot or a dash. The server's `valid_handle` accepts a superset (`@` and `+` too), and `asc` does
/// not link `asc-serve` by default, so this client keeps its own narrower copy.
pub fn valid_reviewer(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_REVIEWER_LEN
        && !name.starts_with(['.', '-'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The reviewers, checked and without repeats (first spelling wins, order kept).
pub fn reviewers(names: &[String]) -> std::result::Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for n in names {
        if !valid_reviewer(n) {
            return Err(format!(
                "asc: submit: '{}' is not a valid user name for --reviewer",
                clean(&n.chars().take(MAX_REVIEWER_LEN + 1).collect::<String>())
            ));
        }
        if !out.iter().any(|o| o.eq_ignore_ascii_case(n)) {
            out.push(n.clone());
        }
    }
    if out.len() > MAX_REVIEWERS {
        return Err(format!(
            "asc: submit: at most {MAX_REVIEWERS} reviewers per submit"
        ));
    }
    Ok(out)
}

/// The AGit ref of one change: `refs/for/<trunk>/asc-<change>`.
pub fn agit_ref(trunk: &str, change: &ChangeId) -> String {
    format!("refs/for/{trunk}/asc-{change}")
}

/// What a submit asks of the server's hooks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wanted {
    pub draft: bool,
    pub reviewers: Vec<String>,
    pub update_only: bool,
    pub auto_merge: bool,
    pub force: bool,
}

/// The push options for `w`, in a fixed order.
pub fn push_options(w: &Wanted) -> Vec<String> {
    let mut o = vec![format!("asc.draft={}", w.draft)];
    if !w.reviewers.is_empty() {
        o.push(format!("asc.reviewers={}", w.reviewers.join(",")));
    }
    if w.update_only {
        o.push("asc.update_only=true".into());
    }
    if w.auto_merge {
        o.push("asc.auto_merge=true".into());
    }
    if w.force {
        o.push("force-push".into());
    }
    o
}

/// The server's free text as printable lines: control characters become `?`, and both the
/// number and the length of the lines are bounded.
pub fn server_lines(text: &str) -> Vec<String> {
    text.lines()
        .take(MAX_MESSAGE_LINES)
        .map(|l| clean(&l.chars().take(MAX_MESSAGE_LINE).collect::<String>()))
        .filter(|l| !l.trim().is_empty())
        .collect()
}

/// Extra words for the errors the server answers with a code.
pub fn hint_for(error_text: &str) -> Option<&'static str> {
    if error_text.contains("CHANGE_DIVERGED") {
        Some(
            "hint: the server holds a version of this change that yours does not replace; run 'asc sync', or submit with --force to replace it",
        )
    } else if error_text.contains("CHANGE_NOT_ALLOWED") {
        Some("hint: you may not update that change's pull request")
    } else if error_text.contains("CHANGE_FROZEN") {
        Some("hint: the change is frozen on the server")
    } else {
        None
    }
}

// ------------------------------------------------------------------------------ JSON

#[derive(Serialize)]
struct ReviewOut {
    number: u64,
    url: String,
    state: String,
    draft: bool,
}

#[derive(Serialize)]
struct ChangeOut {
    change: String,
    short: String,
    commit: String,
    asc: String,
    subject: String,
    #[serde(rename = "ref")]
    agit_ref: String,
    /// Present when the server reported a PR for the change.
    #[serde(skip_serializing_if = "Option::is_none")]
    review: Option<ReviewOut>,
}

#[derive(Serialize)]
struct Out {
    version: u32,
    /// `submitted`, `dry_run`, `nothing` or `ambiguous`.
    status: &'static str,
    remote: String,
    trunk: Option<String>,
    draft: bool,
    reviewers: Vec<String>,
    changes: Vec<ChangeOut>,
    /// The server's message, one entry per line.
    message: Vec<String>,
    /// For `ambiguous`: the tops of the forks.
    candidates: Vec<String>,
}

fn emit(a: &Args, out: &Out) -> Result<()> {
    if a.json {
        let text = line(out).map_err(fatal)?;
        print!("{text}");
        return Ok(());
    }
    match out.status {
        "nothing" => println!("Nothing to submit."),
        "ambiguous" => {
            eprintln!("asc: submit: the stack forks; name the change to submit:");
            for c in &out.candidates {
                eprintln!("  {c}");
            }
        }
        status => {
            let verb = if status == "dry_run" {
                "Would submit"
            } else {
                "Submitted"
            };
            let kind = if out.draft { "draft" } else { "ready" };
            let n = out.changes.len();
            let plural = if n == 1 { "change" } else { "changes" };
            println!("{verb} {n} {plural} to {} ({kind})", clean(&out.remote));
            for c in &out.changes {
                let pr = c.review.as_ref().map_or_else(String::new, |r| {
                    format!(
                        "  #{} {}",
                        r.number,
                        clean(&r.url.chars().take(512).collect::<String>())
                    )
                });
                println!("  {} {}{pr}", c.short, clean(&c.subject));
            }
            for l in &out.message {
                eprintln!("remote: {l}");
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------ planning

/// One change to send.
struct Item {
    cs: ChangesetId,
    change: ChangeId,
    subject: String,
    commit: String,
}

fn share_remote(ctx: &Ctx) -> String {
    ctx.config
        .get("remote.pushDefault")
        .unwrap_or_else(|| "origin".to_owned())
}

/// The changes from the bottom of the stack up to `target`, and what sits above it on each
/// branch: `Err(tops)` when the chains above `target` fork (the tops are the heads of the
/// branches).
async fn chain(
    st: &LStacks<'_, '_>,
    target: ChangesetId,
    whole: bool,
) -> Result<std::result::Result<Vec<ChangesetId>, Vec<ChangesetId>>> {
    let mut heads: Vec<ChangesetId> = st.current_head().into_iter().collect();
    heads.extend(st.heads().iter().copied());
    let mut seen = HashSet::new();
    let mut lower: Option<Vec<ChangesetId>> = None;
    let mut chains: Vec<Vec<ChangesetId>> = Vec::new();
    for h in heads {
        if !seen.insert(h) {
            continue;
        }
        let stack = st.stack_of(h).await.map_err(stack_err)?;
        let Some(i) = stack.changes.iter().position(|&c| c == target) else {
            continue;
        };
        lower.get_or_insert_with(|| stack.changes[..=i].to_vec());
        let above = stack.changes[i + 1..].to_vec();
        if !above.is_empty() && !chains.contains(&above) {
            chains.push(above);
        }
    }
    let Some(mut out) = lower else {
        return Err(CliError::Raw(
            "asc: submit: the change is not on the stack of any branch".into(),
            1,
        ));
    };
    if !whole {
        return Ok(Ok(out));
    }
    chains.sort_by_key(|c| std::cmp::Reverse(c.len()));
    let Some(longest) = chains.first().cloned() else {
        return Ok(Ok(out));
    };
    if chains.iter().any(|c| !longest.starts_with(c)) {
        let mut tops: Vec<ChangesetId> = Vec::new();
        for c in &chains {
            let top = *c.last().expect("chains are not empty");
            if !chains.iter().any(|o| o.len() > c.len() && o.starts_with(c)) && !tops.contains(&top)
            {
                tops.push(top);
            }
        }
        return Ok(Err(tops));
    }
    out.extend(longest);
    Ok(Ok(out))
}

/// The reasons a stack cannot be submitted, checked against the local change index.
async fn refusals(
    l: &Local<'_>,
    st: &LStacks<'_, '_>,
    items: &[(ChangesetId, ChangeId)],
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for (cs, change) in items {
        let short = change.short();
        if st.needs_restack(*cs).await.map_err(stack_err)? {
            out.push(format!("change {short} needs a restack (run 'asc restack')"));
        }
        if st.is_divergent(*change).await.map_err(stack_err)? {
            out.push(format!(
                "change {short} is divergent (several visible versions; abandon all but one)"
            ));
        }
    }
    // A stack that rests on something mutable that is not a change (a merge) cannot be
    // reviewed change by change (SPEC §20.3 ends a stack at a merge).
    if let Some((bottom, _)) = items.first() {
        let node = l.graph.node(*bottom).await.map_err(fatal)?;
        if let Some(&p) = node.parents.first()
            && !st.is_immutable(p).await.map_err(stack_err)?
            && !st.is_draft(p).await.map_err(stack_err)?
        {
            out.push(
                "the stack contains a merge commit; rebase it or submit changes individually"
                    .into(),
            );
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------------------ server reads

fn status_text(e: &CallError) -> String {
    clean(&e.to_string())
}

/// `Changes.ResolveChanges` for `changes`, retried once with a new credential after a 401.
async fn resolve_on_server(
    server: &ServerRemote,
    changes: &[ChangeId],
) -> std::result::Result<Vec<ChangeInfo>, CallError> {
    let mut retried = false;
    loop {
        let session = server.session();
        let mut client = ChangesClient::new(session.wire.channel.clone())
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE);
        let req = wire::ResolveChangesRequest {
            repo: Some(wire::RepoRef {
                name: session.wire.repo.clone(),
                id_mode: IdMode::Asc.to_wire(),
            }),
            changes: changes
                .iter()
                .map(|c| bytes::Bytes::copy_from_slice(c.as_bytes()))
                .collect(),
            revs: Vec::new(),
        };
        match client
            .resolve_changes(session.wire.request(req, None))
            .await
        {
            Ok(r) => {
                crate::credential::report_result(&session.resolved, true);
                let r = r.into_inner();
                if r.changes.len() > MAX_CHANGES * 2 {
                    return Err(CallError::Protocol("too many ChangeInfo rows".into()));
                }
                return change_infos(&r.changes).map_err(CallError::from);
            }
            Err(s) if s.code() == Code::Unauthenticated && !retried => {
                retried = true;
                if server.reauth(&session).is_err() {
                    return Err(CallError::from(s));
                }
            }
            Err(s) => return Err(CallError::from(s)),
        }
    }
}

/// Whether the server has no `Changes` service at all.
fn lacks_service(e: &CallError) -> bool {
    matches!(
        e,
        CallError::Status {
            code: Code::Unimplemented,
            ..
        }
    )
}

fn review_of(info: &ChangeInfo) -> Option<ReviewOut> {
    info.review.as_ref().map(|r| ReviewOut {
        number: r.number,
        url: clean(&r.url),
        state: clean(&r.state),
        draft: r.draft,
    })
}

// ------------------------------------------------------------------------------ run

fn plain_remote_refusal() -> CliError {
    CliError::Raw(
        "asc: submit needs an asc server (not available on a plain git remote)".into(),
        128,
    )
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let reviewers = reviewers(&a.reviewer).map_err(CliError::Usage)?;
    let draft = if a.publish {
        false
    } else if a.draft {
        true
    } else {
        ctx.config.bool("asc.stack.submit.draft").unwrap_or(true)
    };
    let ws = ctx.ws()?;
    let remote_name = share_remote(ctx);
    let opened = remote::open(ctx, ws, &remote_name, Intent::Write).await?;
    let Some(server) = opened.server.clone() else {
        return Err(plain_remote_refusal());
    };

    // The local plan: which changes, in which order, under which trunk.
    let l = Local::open(ctx).await?;
    let st = l.stacks().await?;
    let target = match resolve_change(ctx, &l, &st, a.change.as_deref()).await {
        Ok((cs, _)) => cs,
        Err(CliError::Raw(_, 1)) if a.change.is_none() => {
            let out = Out {
                version: VERSION,
                status: "nothing",
                remote: opened.name.clone(),
                trunk: None,
                draft,
                reviewers,
                changes: Vec::new(),
                message: Vec::new(),
                candidates: Vec::new(),
            };
            emit(&a, &out)?;
            return Ok(ExitCode::SUCCESS);
        }
        Err(e) => return Err(e),
    };
    let css = match chain(&st, target, a.stack).await? {
        Ok(v) => v,
        Err(tops) => {
            let mut candidates = Vec::new();
            for t in tops {
                if let Some(row) = st.draft_row(t).await.map_err(stack_err)? {
                    candidates.push(row.change.to_string());
                }
            }
            let out = Out {
                version: VERSION,
                status: "ambiguous",
                remote: opened.name.clone(),
                trunk: None,
                draft,
                reviewers,
                changes: Vec::new(),
                message: Vec::new(),
                candidates,
            };
            emit(&a, &out)?;
            return Ok(ExitCode::from(2));
        }
    };
    if css.len() > MAX_CHANGES {
        return Err(CliError::Fatal(format!(
            "asc: submit: {} changes is over the limit of {MAX_CHANGES}; submit a lower change first",
            css.len()
        )));
    }
    let mut rows: Vec<(ChangesetId, ChangeId)> = Vec::with_capacity(css.len());
    for cs in &css {
        let row = st
            .draft_row(*cs)
            .await
            .map_err(stack_err)?
            .ok_or_else(|| CliError::Fatal("asc: submit: not a draft change".into()))?;
        rows.push((*cs, row.change));
    }
    let mut problems = refusals(&l, &st, &rows).await?;
    let top = *css
        .last()
        .ok_or_else(|| CliError::Fatal("asc: submit: nothing to submit".into()))?;
    let trunk_tip = st
        .stack_of(top)
        .await
        .map_err(stack_err)?
        .trunk
        .ok_or_else(|| {
            CliError::Fatal(format!(
                "asc: submit: the stack has no trunk: asc.stack.trunk is {:?} and no such branch exists here (set it to the branch the server calls its default; a clone sets it)",
                l.env.trunk_names()
            ))
        })?;
    let trunk = l
        .env
        .trunk_info()
        .into_iter()
        .find(|t| t.tip == trunk_tip)
        .map(|t| t.name)
        .ok_or_else(|| CliError::Fatal("asc: submit: the stack has no trunk".into()))?;

    // What the server already knows about these changes.
    let ids: Vec<ChangeId> = rows.iter().map(|r| r.1).collect();
    let mut known: HashMap<ChangeId, ChangeInfo> = HashMap::new();
    let mut have_info = false;
    match resolve_on_server(&server, &ids).await {
        Ok(infos) => {
            have_info = true;
            for i in infos {
                known.insert(i.change, i);
            }
        }
        Err(e) if lacks_service(&e) => {
            if a.update_only {
                return Err(CliError::Fatal(
                    "asc: submit: --update-only needs the server's Changes service".into(),
                ));
            }
        }
        Err(e) if a.dry_run && !a.update_only => {
            let _ = e;
        }
        Err(e) => {
            return Err(CliError::Fatal(format!(
                "asc: submit: could not ask the server about these changes: {}",
                status_text(&e)
            )));
        }
    }
    for (_, change) in &rows {
        if let Some(i) = known.get(change) {
            let short = change.short();
            if i.landed_as.is_some() {
                problems.push(format!("change {short} has already landed (run 'asc sync')"));
            } else if i
                .review
                .as_ref()
                .is_some_and(|r| r.state.eq_ignore_ascii_case("merged"))
            {
                problems.push(format!(
                    "the pull request of change {short} is already merged (run 'asc sync')"
                ));
            }
        }
    }
    if !problems.is_empty() {
        let mut text = String::from("asc: submit: refusing to submit:");
        for p in &problems {
            text.push_str("\n  ");
            text.push_str(&clean(p));
        }
        return Err(CliError::Fatal(text));
    }

    // `--update-only` sends the changes that have a PR and nothing else.
    let rows: Vec<(ChangesetId, ChangeId)> = if a.update_only {
        if !have_info {
            return Err(CliError::Fatal(
                "asc: submit: --update-only needs the server to report the existing PRs".into(),
            ));
        }
        rows.into_iter()
            .filter(|(_, c)| known.get(c).is_some_and(|i| i.review.is_some()))
            .collect()
    } else {
        rows
    };

    let mut items: Vec<Item> = Vec::with_capacity(rows.len());
    for (cs, change) in &rows {
        let c = ws.engine.get_changeset(*cs).await?;
        items.push(Item {
            cs: *cs,
            change: *change,
            subject: subject_of(&c.message),
            commit: l.git_id(*cs).await?,
        });
    }
    drop(st);
    drop(l);

    let build = |reviews: &HashMap<ChangeId, ChangeInfo>, items: &[Item]| -> Vec<ChangeOut> {
        items
            .iter()
            .map(|it| ChangeOut {
                change: it.change.to_string(),
                short: it.change.short(),
                commit: it.commit.clone(),
                asc: it.cs.to_string(),
                subject: it.subject.clone(),
                agit_ref: agit_ref(&trunk, &it.change),
                review: reviews.get(&it.change).and_then(review_of),
            })
            .collect()
    };
    let mut out = Out {
        version: VERSION,
        status: "submitted",
        remote: opened.name.clone(),
        trunk: Some(trunk.clone()),
        draft,
        reviewers: reviewers.clone(),
        changes: Vec::new(),
        message: Vec::new(),
        candidates: Vec::new(),
    };
    if items.is_empty() {
        out.status = "nothing";
        emit(&a, &out)?;
        return Ok(ExitCode::SUCCESS);
    }
    if a.dry_run {
        out.status = "dry_run";
        out.changes = build(&known, &items);
        emit(&a, &out)?;
        return Ok(ExitCode::SUCCESS);
    }

    // The updates: validated ref names, bottom to top, in one call.
    let mut updates: Vec<PushUpdate> = Vec::with_capacity(items.len());
    for it in &items {
        updates.push(PushUpdate {
            dst: ref_name(&agit_ref(&trunk, &it.change))?,
            new: Some(RefTarget::Changeset(it.cs)),
            expect: Expect::Any,
        });
    }

    // `m/` objects travel only when the server says it understands them (C23).
    let features = match server.client().server_version().await {
        Ok(v) => v.features,
        Err(_) => Vec::new(),
    };
    let ext = ChangesExt::new(StateChanges::new(ws.db.clone(), ws.repo.key), &features);
    let new_css: Vec<ChangesetId> = items.iter().map(|i| i.cs).collect();
    ext.warm(&new_css).await.map_err(fatal)?;

    let boundaries = StateBoundaries::new(ws);
    let mut sync_opts = remote::options(ctx, &boundaries);
    sync_opts.ext = &ext;
    server.arm(PushIntent {
        force: a.force,
        land: false,
        no_content_merge: false,
        options: push_options(&Wanted {
            draft,
            reviewers: reviewers.clone(),
            update_only: a.update_only,
            auto_merge: a.auto_merge,
            force: a.force,
        }),
    });
    let pushed = ws
        .engine
        .push(&*opened.remote, &updates, PushMode::Force, &sync_opts)
        .await;
    let outcome = server.take_outcome();
    server.arm(PushIntent::default());
    if let Err(e) = pushed {
        let text = e.to_string();
        let hint = hint_for(&text);
        return Err(match sync_error(e) {
            CliError::Fatal(m) => CliError::Fatal(match hint {
                Some(h) => format!("{}\n{h}", clean_keep_lines(&m)),
                None => clean_keep_lines(&m),
            }),
            other => other,
        });
    }

    // The PR numbers and links, from the server's index (the push message is the fallback).
    let mut after: HashMap<ChangeId, ChangeInfo> = HashMap::new();
    if let Ok(infos) = resolve_on_server(&server, &ids_of(&items)).await {
        for i in infos {
            after.insert(i.change, i);
        }
    }
    out.changes = build(&after, &items);
    if let Some(o) = &outcome {
        out.message = server_lines(&o.message);
    }
    emit(&a, &out)?;
    Ok(ExitCode::SUCCESS)
}

fn ids_of(items: &[Item]) -> Vec<ChangeId> {
    items.iter().map(|i| i.change).collect()
}

/// `clean` that keeps the line structure of a multi-line message.
fn clean_keep_lines(s: &str) -> String {
    s.chars()
        .map(|c| if c != '\n' && c.is_control() { '?' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reviewer_names_cannot_change_the_option_they_travel_in() {
        for good in ["alice", "a.b_c-d", "A1", "x"] {
            assert!(valid_reviewer(good), "{good}");
        }
        for bad in [
            "",
            "a,b",
            "a=b",
            "a b",
            "a\nb",
            "a\u{1b}[31m",
            ".hidden",
            "-flag",
            "ünï",
            &"x".repeat(MAX_REVIEWER_LEN + 1),
        ] {
            assert!(!valid_reviewer(bad), "{bad:?}");
        }
    }

    #[test]
    fn reviewers_are_deduplicated_and_bounded() {
        let names: Vec<String> = ["alice", "Bob", "ALICE", "bob"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(reviewers(&names).unwrap(), ["alice", "Bob"]);
        let many: Vec<String> = (0..=MAX_REVIEWERS).map(|i| format!("u{i}")).collect();
        assert!(reviewers(&many).is_err());
        let err = reviewers(&["bad name\u{7}".to_owned()]).unwrap_err();
        assert!(!err.contains('\u{7}'), "{err}");
    }

    #[test]
    fn the_agit_ref_names_the_trunk_and_the_change() {
        let id = ChangeId::from_bytes([3; 16]);
        assert_eq!(agit_ref("master", &id), format!("refs/for/master/asc-{id}"),);
        assert!(ref_name(&agit_ref("release/1.0", &id)).is_ok());
        assert!(ref_name(&agit_ref("bad..trunk", &id)).is_err());
        assert!(ref_name(&agit_ref("bad trunk", &id)).is_err());
    }

    #[test]
    fn options_are_in_a_fixed_order() {
        let mut w = Wanted {
            draft: true,
            reviewers: vec![],
            update_only: false,
            auto_merge: false,
            force: false,
        };
        assert_eq!(push_options(&w), ["asc.draft=true"]);
        w.draft = false;
        w.reviewers = vec!["a".into(), "b".into()];
        w.update_only = true;
        w.auto_merge = true;
        w.force = true;
        assert_eq!(
            push_options(&w),
            [
                "asc.draft=false",
                "asc.reviewers=a,b",
                "asc.update_only=true",
                "asc.auto_merge=true",
                "force-push",
            ]
        );
    }

    #[test]
    fn the_servers_text_is_cleaned_and_bounded() {
        let lines = server_lines("ok\n\u{1b}[2Jgone\n\n   \nlast");
        assert_eq!(lines, ["ok", "?[2Jgone", "last"]);
        let big = "x\n".repeat(MAX_MESSAGE_LINES * 2);
        assert_eq!(server_lines(&big).len(), MAX_MESSAGE_LINES);
        let long = "y".repeat(MAX_MESSAGE_LINE * 3);
        assert_eq!(server_lines(&long)[0].len(), MAX_MESSAGE_LINE);
    }

    #[test]
    fn server_codes_get_a_hint() {
        assert!(
            hint_for("asc: CHANGE_DIVERGED: x")
                .unwrap()
                .contains("--force")
        );
        assert!(hint_for("CHANGE_NOT_ALLOWED").unwrap().contains("may not"));
        assert!(hint_for("CHANGE_FROZEN").unwrap().contains("frozen"));
        assert!(hint_for("something else").is_none());
    }
}
