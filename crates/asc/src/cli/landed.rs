//! `asc landed <rev> [--ref <ref>] [--remote <name>] [--json]` (SPEC §8.9): where a change landed.
//!
//! Asks the server's `FindLanding` (the landing map and the ref log), never walks the log or
//! matches messages. Exit 0 and one line (or one JSON object) when the change landed, exit 1 when
//! it did not.

use std::ffi::OsString;
use std::process::ExitCode;

use asc_core::id::ChangesetId;
use asc_rpc::convert::WireId;
use asc_rpc::server::wire;
use asc_rpc::services::repo_client::RepoClient;

use super::common::{CliError, Ctx, Result};
use super::revparse::commit_of;
use crate::native::NativeSpec;
use crate::remote::{self, Intent};

/// Largest message the reply may carry; the answer is a few hundred bytes.
const MAX_MESSAGE: usize = 1 << 20;

#[derive(clap::Parser, Debug)]
#[command(name = "landed", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// The server ref to look on; the default branch when absent.
    #[arg(long = "ref")]
    pub refname: Option<String>,
    /// The remote to ask; `origin` when absent.
    #[arg(long)]
    pub remote: Option<String>,
    #[arg(long)]
    pub json: bool,
    /// The revision, then anything else (a usage error).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "landed",
    flags: &["--ref", "--remote", "--json"],
    placeholders: &[],
}];

fn usage(text: &str) -> CliError {
    CliError::Usage(format!(
        "usage: asc landed <rev> [--ref <ref>] [--remote <name>] [--json]\n{text}"
    ))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `secs` since the epoch as `YYYY-MM-DDTHH:MM:SSZ`.
fn rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil date from a day count (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let mut words = a.rest.iter();
    let Some(word) = words.next() else {
        return Err(usage("error: a revision is required"));
    };
    if let Some(extra) = words.next() {
        return Err(usage(&format!(
            "error: unexpected argument '{}'",
            extra.to_string_lossy()
        )));
    }
    let word = word
        .to_str()
        .ok_or_else(|| usage("error: the revision is not UTF-8"))?;
    let ws = ctx.ws()?;
    let cs: ChangesetId = commit_of(ctx, word)
        .await?
        .ok_or_else(|| CliError::Fatal(format!("bad revision '{word}'")))?;
    let name = a.remote.as_deref().unwrap_or("origin");
    let opened = remote::open(ctx, ws, name, Intent::Read).await?;
    let Some(server) = opened.server.as_ref() else {
        return Err(CliError::Fatal(format!(
            "'{name}' is not an asc server; landing is recorded by the server"
        )));
    };
    let wire_conn = server.session().wire.clone();
    let mut stub = RepoClient::new(wire_conn.channel.clone())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE);
    let request = wire_conn.request(
        wire::FindLandingRequest {
            repo: Some(wire::RepoRef {
                name: wire_conn.repo.clone(),
                id_mode: asc_rpc::IdMode::Asc.to_wire(),
            }),
            changeset: bytes::Bytes::copy_from_slice(cs.as_bytes()),
            r#ref: a.refname.clone().unwrap_or_default(),
        },
        None,
    );
    let reply = stub
        .find_landing(request)
        .await
        .map_err(|s| CliError::Fatal(format!("asc: {}", s.message())))?
        .into_inner();
    if !reply.found {
        if a.json {
            println!("{}", serde_json::json!({ "landed": false }));
        } else {
            println!("{word} has not landed");
        }
        return Ok(ExitCode::from(1));
    }
    let landed = ChangesetId::from_slice(&reply.landed_as)
        .map_err(|e| CliError::Fatal(format!("asc: the server sent a bad id: {e}")))?;
    // The git id when this machine has the changeset; the asc id otherwise.
    let git = match ws.engine.git_commit_of(landed).await {
        Ok(g) => Some(g.to_string()),
        Err(_) => None,
    };
    if a.json {
        println!(
            "{}",
            serde_json::json!({
                "landed": true,
                "landed_as": hex(landed.as_bytes()),
                "git": git,
                "ref_log_seq": reply.ref_log_seq,
                "at": reply.at,
                "actor": reply.actor,
                "via": reply.via,
            })
        );
    } else {
        let shown = git.unwrap_or_else(|| hex(landed.as_bytes()));
        println!(
            "{word} landed as {shown} at {} by {} (ref log #{})",
            rfc3339(reply.at),
            reply.actor,
            reply.ref_log_seq
        );
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::rfc3339;

    #[test]
    fn dates_are_civil_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339(-1), "1969-12-31T23:59:59Z");
    }
}
