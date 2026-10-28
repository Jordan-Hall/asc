//! `asc share [-m <msg>]` (SPEC §20.7, §8.8): snapshots the working changes, uploads the
//! snapshot and its history to the server and roots it by
//! `refs/snapshots/<id>` with an expiry (default 30 days). Prints `asc:<id>` and a web link;
//! a recipient runs `asc stack get asc:<id>`.
//!
//! The snapshot is the stash commit of the work tree (tracked changes, the index and
//! untracked files) on top of HEAD, or HEAD itself when nothing is changed. `<id>` is the git
//! SHA-1 of that commit, so the ref name is derived from content and a re-share of the same
//! state only refreshes the expiry. Nothing in the work tree, the index or any local ref moves.

use std::process::ExitCode;

use asc_core::id::RefTarget;
use asc_engine::sync::{PushMode, PushUpdate};
use asc_store::refs::{Expect, RefUpdate};
use serde::Serialize;

use crate::cli::common::{CliError, Ctx, Result};
use crate::cli::conflict;
use crate::cli::fetch::sync_error;
use crate::cli::stash::save::{self, Opts};
use crate::cli::switch::ref_name;
use crate::native::NativeSpec;
use crate::remote::{self, Intent, StateBoundaries};
use crate::stack::json::{VERSION, line};

/// Default life of a shared snapshot (SPEC §20.7).
pub const DEFAULT_TTL_SECS: i64 = 30 * 24 * 3600;
/// Longest message `-m` takes.
const MAX_MESSAGE: usize = 64 * 1024;

#[derive(clap::Parser, Debug)]
#[command(name = "share", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// The snapshot's message.
    #[arg(short = 'm', long = "message")]
    pub message: Option<String>,
    #[arg(long)]
    pub json: bool,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "share",
    flags: &["-m", "--message", "--json"],
    placeholders: &[],
}];

super::verb!(SHARE, "share");
/// `asc stack share` is the same command as `asc share` (SPEC 20.7 lists the stack verbs under both spellings).
super::info::sub!(STACK_SHARE, "share", Args, run);

#[derive(Serialize)]
struct Out {
    version: u32,
    id: String,
    link: String,
    expires_at: i64,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

/// The remote `share` uploads to.
fn share_remote(ctx: &Ctx) -> String {
    ctx.config
        .get("remote.pushDefault")
        .unwrap_or_else(|| "origin".to_owned())
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    if a.message
        .as_deref()
        .is_some_and(|m| m.len() > MAX_MESSAGE || m.contains('\0'))
    {
        return Err(CliError::Usage(
            "asc: share: the message is too long or holds a NUL".into(),
        ));
    }
    let opened = remote::open(ctx, ws, &share_remote(ctx), Intent::Write).await?;
    let Some(server) = opened.server.clone() else {
        return Err(CliError::Raw(
            "asc: share needs an asc server (not available on a plain git remote)".into(),
            128,
        ));
    };

    let opts = Opts {
        untracked: true,
        fold_untracked: true,
        quiet: true,
        verbatim: true,
        message: Some(a.message.clone().unwrap_or_else(|| "asc share".to_owned())),
        ..Opts::default()
    };
    let commit = match save::make(ctx, ws, &opts, false).await? {
        Some(made) => made.commit,
        None => ws.head_commit().await?.ok_or_else(|| {
            CliError::Fatal("asc: share: there is nothing to share (no commit yet)".into())
        })?,
    };
    let id = conflict::git_id(ws, commit).await?.to_string();
    let name = ref_name(&format!("refs/snapshots/{id}"))?;

    // The closure goes up with the ref (`UpdateRefs` ingests it, SPEC §8.3), then the same call
    // sets the expiry. The ref is never left without one: a failed second step deletes it.
    let boundaries = StateBoundaries::new(ws);
    let sync_opts = remote::options(ctx, &boundaries);
    ws.engine
        .push(
            &*opened.remote,
            &[PushUpdate {
                dst: name.clone(),
                new: Some(RefTarget::Changeset(commit)),
                expect: Expect::Any,
            }],
            PushMode::Force,
            &sync_opts,
        )
        .await
        .map_err(sync_error)?;
    let expires_at = now_secs().saturating_add(DEFAULT_TTL_SECS);
    let rooted = opened
        .remote
        .update_refs(&[RefUpdate {
            name: name.clone(),
            expect: Expect::Any,
            new: Some(RefTarget::Changeset(commit)),
            expires_at: Some(expires_at),
        }])
        .await;
    if let Err(e) = rooted {
        let _ = opened
            .remote
            .update_refs(&[RefUpdate {
                name,
                expect: Expect::Any,
                new: None,
                expires_at: None,
            }])
            .await;
        return Err(sync_error(e));
    }

    let target = server.target();
    let link = format!(
        "{}/{}/snapshots/{id}",
        target.rpc_url.trim_end_matches('/'),
        target.repo
    );
    if a.json {
        let text = line(&Out {
            version: VERSION,
            id: format!("asc:{id}"),
            link,
            expires_at,
        })
        .map_err(|e| CliError::Fatal(format!("asc: {e}")))?;
        print!("{text}");
    } else {
        println!("asc:{id}");
        println!("{link}");
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_expiry_is_thirty_days() {
        assert_eq!(DEFAULT_TTL_SECS, 2_592_000);
    }
}
