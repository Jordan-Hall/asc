//! `asc signing default [on|off] [--remote R] [--json]`: whether every new change is signed without a flag
//! (`Signing.GetSigningDefaults`, `Signing.SetSigningDefaults`). The value lives on the server with the account, so
//! the web settings page and every client agree; this command keeps a copy in the global config key
//! `asc.signByDefault`. Commands that write changes read [`sign_by_default`]: a set `commit.gpgsign` wins, then
//! `asc.signByDefault`, else off.

use std::ffi::OsString;
use std::process::ExitCode;

use asc_rpc::server::wire;
use tonic::transport::Channel;

use super::acct::{bad, json, out};
use super::agent::conn::{Conn, MAX_MESSAGE};
use super::common::{Ctx, Result};
use super::config::ConfigSnapshot;
use crate::native::NativeSpec;
use asc_rpc::services::signing_client::SigningClient;

/// The global config key that keeps the account's signing default.
pub const KEY_SIGN_BY_DEFAULT: &str = "asc.signByDefault";

/// Whether a command that writes a change signs it when no flag says. `commit.gpgsign`, when set at any
/// level, wins; otherwise the account's default as `asc signing default` last saw it.
pub fn sign_by_default(config: &ConfigSnapshot) -> bool {
    config
        .bool("commit.gpgsign")
        .or_else(|| config.bool(KEY_SIGN_BY_DEFAULT))
        .unwrap_or(false)
}

fn client(s: &crate::remote::server::Session) -> SigningClient<Channel> {
    SigningClient::new(s.wire.channel.clone())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE)
}

#[derive(clap::Parser, Debug)]
#[command(
    name = "signing",
    disable_help_flag = true,
    disable_version_flag = true,
    disable_help_subcommand = true
)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Sub,
}

#[derive(clap::Subcommand, Debug)]
pub enum Sub {
    /// Show or set whether new changes are signed by default.
    Default(DefaultArgs),
}

#[derive(clap::Args, Debug)]
pub struct DefaultArgs {
    /// `on` or `off`; shows the current value when absent.
    value: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "signing",
    flags: &["--remote", "--json"],
    placeholders: &[],
}];

pub(crate) fn parse_switch(s: &str) -> Result<bool> {
    match s {
        "on" | "true" | "yes" => Ok(true),
        "off" | "false" | "no" => Ok(false),
        _ => Err(bad("signing default", "the value is on or off")),
    }
}

/// Keeps the account's value in the global config, so commands that run offline still follow it.
async fn remember(ctx: &Ctx, on: bool) -> Result<()> {
    let rest: Vec<OsString> = [
        "--global",
        KEY_SIGN_BY_DEFAULT,
        if on { "true" } else { "false" },
    ]
    .map(OsString::from)
    .to_vec();
    super::config::run(ctx, super::config::Args { rest }).await?;
    Ok(())
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let Sub::Default(d) = a.cmd;
    let want = d.value.as_deref().map(parse_switch).transpose()?;
    let conn = Conn::open(ctx, d.remote.as_deref()).await?;
    let (on, updated) = match want {
        Some(v) => {
            let msg = wire::SetSigningDefaultsRequest { sign_by_default: v };
            let r = conn
                .call(true, |s, op| async move {
                    Ok(client(&s)
                        .set_signing_defaults(s.wire.request(msg, op))
                        .await?
                        .into_inner())
                })
                .await?;
            (r.sign_by_default, r.updated)
        }
        None => {
            let r = conn
                .call(false, |s, _| async move {
                    Ok(client(&s)
                        .get_signing_defaults(
                            s.wire
                                .request(wire::GetSigningDefaultsRequest::default(), None),
                        )
                        .await?
                        .into_inner())
                })
                .await?;
            (r.sign_by_default, r.updated)
        }
    };
    remember(ctx, on).await?;
    if d.json {
        return json(
            "asc.v1.GetSigningDefaultsResponse",
            &wire::GetSigningDefaultsResponse {
                sign_by_default: on,
                updated,
            },
        );
    }
    let overridden = ctx.config.bool("commit.gpgsign").filter(|g| *g != on);
    out(|out| {
        writeln!(out, "sign by default: {}", if on { "on" } else { "off" })?;
        if let Some(g) = overridden {
            writeln!(
                out,
                "commit.gpgsign is {} here and wins over the account default",
                if g { "true" } else { "false" }
            )?;
        }
        Ok(ExitCode::SUCCESS)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_parse() {
        assert!(parse_switch("on").unwrap());
        assert!(!parse_switch("off").unwrap());
        assert!(parse_switch("true").unwrap());
        assert!(parse_switch("maybe").is_err());
        assert!(parse_switch("").is_err());
    }

    #[test]
    fn commit_gpgsign_wins_over_the_account_default() {
        let c = |t: &str| ConfigSnapshot::from_text(t);
        assert!(!sign_by_default(&c("")));
        assert!(sign_by_default(&c("[asc]\n\tsignByDefault = true\n")));
        assert!(!sign_by_default(&c("[asc]\n\tsignByDefault = false\n")));
        assert!(sign_by_default(&c("[commit]\n\tgpgsign = true\n")));
        assert!(!sign_by_default(&c(
            "[commit]\n\tgpgsign = false\n[asc]\n\tsignByDefault = true\n"
        )));
        assert!(sign_by_default(&c(
            "[commit]\n\tgpgsign = true\n[asc]\n\tsignByDefault = false\n"
        )));
    }
}
