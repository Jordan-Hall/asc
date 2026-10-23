//! `asc watch [--since <token>] [--json]` (SPEC §7, §11.2): streams the daemon's `Watch` events
//! of the current workspace, one `WatchEvent` per line (JSON lines with `--json`, else one text
//! line). Every event carries the token that resumes right after it, so `--since <token>` of the
//! last event received continues without loss. The command needs the daemon (it is the only
//! process that sees the work tree change); it starts it and mounts the workspace first.

use std::io::Write;
use std::process::ExitCode;

use asc_rpc::ipc::{WorkspaceClient, connect};
use asc_rpc::server::wire;
use prost_reflect::{DescriptorPool, DynamicMessage};

use super::common::{CliError, Ctx, Result};
use super::json::JsonLines;
use crate::daemon::lifecycle;
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "watch", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Resume after the event that carried this token.
    #[arg(long)]
    pub since: Option<String>,
    /// Print `WatchEvent`s as JSON lines (the asc JSON profile).
    #[arg(long)]
    pub json: bool,
    /// Anything else is a usage error.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<std::ffi::OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "watch",
    flags: &["--since", "--json"],
    placeholders: &[],
}];

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("asc: {e}"))
}

/// The text form of one event: `<kind> <ref|op> <paths…> <token>`.
fn text_line(e: &wire::WatchEvent) -> String {
    let kind = wire::watch_event::Kind::try_from(e.kind).map_or("UNKNOWN", |k| k.as_str_name());
    let paths: Vec<String> = e
        .paths
        .iter()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    let mut out = kind.to_owned();
    for part in [e.r#ref.as_str(), e.op.as_str()] {
        if !part.is_empty() {
            out.push(' ');
            out.push_str(part);
        }
    }
    if !paths.is_empty() {
        out.push(' ');
        out.push_str(&paths.join(" "));
    }
    out.push('\t');
    out.push_str(&e.token);
    out
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    if !args.rest.is_empty() {
        return Err(CliError::Usage(
            "error: watch takes no arguments but --since and --json".into(),
        ));
    }
    let ws = ctx.ws()?;
    let ws_id = ws.workspace.ws.id.to_string();
    if lifecycle::disabled(&|k: &str| std::env::var_os(k)) {
        return Err(fatal(
            "watch needs the daemon, which ASC_NO_DAEMON turns off",
        ));
    }
    let (cfg, proc) = super::daemon::system()?;
    lifecycle::ensure_running(&cfg, &proc)
        .await
        .map_err(fatal)?;
    let mut client = WorkspaceClient::new(connect(&cfg.endpoint));
    client
        .mount(wire::MountRequest {
            ws_id: ws_id.clone(),
        })
        .await
        .map_err(|s| fatal(s.message()))?;
    let mut events = client
        .watch(wire::WatchRequest {
            ws_id,
            since_token: args.since.unwrap_or_default(),
        })
        .await
        .map_err(|s| fatal(s.message()))?
        .into_inner();

    let pool = DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).map_err(fatal)?;
    let desc = pool
        .get_message_by_name("asc.v1.WatchEvent")
        .ok_or_else(|| fatal("missing message asc.v1.WatchEvent"))?;
    loop {
        let event = match events.message().await {
            Ok(Some(e)) => e,
            Ok(None) => return Err(fatal("the daemon closed the watch stream")),
            Err(s) => return Err(fatal(s.message())),
        };
        let line = if args.json {
            let mut msg = DynamicMessage::new(desc.clone());
            msg.transcode_from(&event).map_err(fatal)?;
            let mut lines = JsonLines::new(Vec::new());
            lines.emit(&msg).map_err(fatal)?;
            lines.finish().map_err(CliError::Io)?
        } else {
            format!(
                "{}
",
                text_line(&event)
            )
            .into_bytes()
        };
        // One event per write, flushed at once; a closed pipe is the reader's choice.
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        match lock.write_all(&line).and_then(|()| lock.flush()) {
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => return Ok(ExitCode::SUCCESS),
            Err(e) => return Err(CliError::Io(e)),
            Ok(()) => {}
        }
    }
}
