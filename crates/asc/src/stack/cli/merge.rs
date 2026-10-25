//! `asc stack merge [<change>] [--now]` (SPEC §20.7, §20.5 flow 5): asks the server to merge
//! bottom..`<change>` (default the current change) when it is ready, by `Changes.RequestMerge`.
//! `--now` asks to land at once when every PR of the prefix is mergeable. The CLI never calls a
//! forge API (D9): the server relays the request to its queue, or lands directly with no forge
//! attached. This is not `asc landed` (§8.9), which only reads what already landed.
//!
//! * The change is resolved locally and sent by its change id; the server decides what the
//!   prefix is and who may merge it.
//! * A plain git remote has no `Changes` service: exit 128 with the Stage 0 text.
//! * `STATE_QUEUED` and `STATE_LANDED` exit 0; `STATE_REFUSED` exits 1 with the server's reason.
//! * Everything the server answers is printed with control characters replaced and bounded.
//! * One operation id per run, so a retried call after a 401 is the same logical call.

use std::process::ExitCode;

use asc_rpc::IdMode;
use asc_rpc::changes::{MergeOutcome, MergeRequestState};
use asc_rpc::client::CallError;
use asc_rpc::server::{MAX_MESSAGE, wire};
use asc_rpc::services::changes_client::ChangesClient;
use serde::Serialize;
use tonic::Code;
use uuid::Uuid;

use super::info::resolve_change;
use super::sl::{Local, clean, fatal};
use crate::cli::common::{CliError, Ctx, Result};
use crate::remote::server::ServerRemote;
use crate::remote::{self, Intent};
use crate::stack::json::{VERSION, line};

/// Most pending changes printed.
const MAX_PENDING: usize = 256;
/// Longest printed reason.
const MAX_REASON: usize = 1024;

#[derive(clap::Parser, Debug)]
#[command(name = "merge", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Merge up to this change; default the current change.
    pub change: Option<String>,
    /// Land at once when every PR in the prefix is mergeable.
    #[arg(long)]
    pub now: bool,
    #[arg(long)]
    pub json: bool,
}

super::info::sub!(STACK_MERGE, "merge", Args, run);

#[derive(Serialize)]
struct Out {
    version: u32,
    change: String,
    state: &'static str,
    pending: Vec<String>,
    reason: String,
}

fn state_word(s: MergeRequestState) -> &'static str {
    match s {
        MergeRequestState::Queued => "queued",
        MergeRequestState::Landed => "landed",
        MergeRequestState::Refused => "refused",
    }
}

fn out_of(change: String, o: &MergeOutcome) -> Out {
    Out {
        version: VERSION,
        change,
        state: state_word(o.state),
        pending: o
            .pending
            .iter()
            .take(MAX_PENDING)
            .map(ToString::to_string)
            .collect(),
        reason: clean(&o.reason.chars().take(MAX_REASON).collect::<String>()),
    }
}

/// `Changes.RequestMerge`, retried once with a new credential after a 401 (same operation id).
async fn request(
    server: &ServerRemote,
    change: &asc_core::change::ChangeId,
    now: bool,
) -> std::result::Result<MergeOutcome, CallError> {
    let op = Uuid::new_v4();
    let mut retried = false;
    loop {
        let session = server.session();
        let mut client = ChangesClient::new(session.wire.channel.clone())
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE);
        let req = wire::RequestMergeRequest {
            repo: Some(wire::RepoRef {
                name: session.wire.repo.clone(),
                id_mode: IdMode::Asc.to_wire(),
            }),
            change: bytes::Bytes::copy_from_slice(change.as_bytes()),
            now,
        };
        match client
            .request_merge(session.wire.request(req, Some(op)))
            .await
        {
            Ok(r) => {
                crate::credential::report_result(&session.resolved, true);
                let r = r.into_inner();
                return MergeOutcome::try_from(&r).map_err(CallError::from);
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

fn remote_name(ctx: &Ctx) -> String {
    ctx.config
        .get("remote.pushDefault")
        .unwrap_or_else(|| "origin".to_owned())
}

async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let l = Local::open(ctx).await?;
    let st = l.stacks().await?;
    let (_, row) = resolve_change(ctx, &l, &st, a.change.as_deref()).await?;
    let change = row.change;

    let opened = remote::open(ctx, ws, &remote_name(ctx), Intent::Write).await?;
    let Some(server) = opened.server.clone() else {
        return Err(CliError::Raw(
            "asc: stack merge needs an asc server (not available on a plain git remote)".into(),
            128,
        ));
    };
    let outcome = match request(&server, &change, a.now).await {
        Ok(o) => o,
        Err(CallError::Status {
            code: Code::Unimplemented,
            ..
        }) => {
            return Err(CliError::Raw(
                "asc: stack merge needs an asc server (not available on a plain git remote)".into(),
                128,
            ));
        }
        Err(e) => {
            return Err(CliError::Fatal(format!(
                "asc: stack merge: {}",
                clean(&e.to_string())
            )));
        }
    };
    let out = out_of(change.to_string(), &outcome);
    if a.json {
        print!("{}", line(&out).map_err(fatal)?);
    } else {
        let short: String = out.change.chars().take(12).collect();
        match outcome.state {
            MergeRequestState::Landed => println!("Landed {short} and the changes below it."),
            MergeRequestState::Queued => {
                println!("Queued {short} to merge when ready.");
                for p in &out.pending {
                    println!("  waiting: {}", p.chars().take(12).collect::<String>());
                }
            }
            MergeRequestState::Refused => {
                eprintln!("asc: stack merge: refused: {}", out.reason);
            }
        }
    }
    Ok(if outcome.state == MergeRequestState::Refused {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use asc_core::change::ChangeId;

    #[test]
    fn the_reason_is_cleaned_and_bounded() {
        let o = MergeOutcome {
            state: MergeRequestState::Refused,
            pending: vec![ChangeId::from_bytes([1; 16])],
            reason: format!("a\u{1b}[31mb{}", "x".repeat(5000)),
        };
        let out = out_of("c".into(), &o);
        assert!(!out.reason.contains('\u{1b}'));
        assert!(out.reason.chars().count() <= MAX_REASON);
        assert_eq!(out.state, "refused");
        assert_eq!(out.pending.len(), 1);
    }

    #[test]
    fn merge_is_registered() {
        assert!(super::super::info::SUBS.iter().any(|s| s.name == "merge"));
    }
}
