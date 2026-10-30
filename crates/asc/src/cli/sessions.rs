//! `asc sessions list | revoke <id> | others [--keep <id>]`: the tokens, SSH keys and agent tokens of
//! your account and how to sign them out (`Sessions`).

use std::process::ExitCode;

use asc_rpc::server::wire;
use tonic::transport::Channel;

use super::acct::{bad, json};
use super::acct::{clean_str, out, warn_};
use super::agent::conn::{Conn, MAX_MESSAGE, when};
use super::common::{Ctx, Result};
use crate::native::NativeSpec;
use asc_rpc::services::sessions_client::SessionsClient;

fn client(s: &crate::remote::server::Session) -> SessionsClient<Channel> {
    SessionsClient::new(s.wire.channel.clone())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE)
}

#[derive(clap::Parser, Debug)]
#[command(
    name = "sessions",
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
    List(Common),
    Revoke(IdArgs),
    /// Signs out everything but the credential in use.
    Others(OthersArgs),
}

#[derive(clap::Args, Debug)]
pub struct Common {
    /// Another account (site administrators only).
    #[arg(long)]
    login: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct IdArgs {
    id: String,
    #[command(flatten)]
    common: Common,
}

#[derive(clap::Args, Debug)]
pub struct OthersArgs {
    #[arg(long)]
    keep: Option<String>,
    #[command(flatten)]
    common: Common,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "sessions",
    flags: &["--login", "--remote", "--json", "--keep"],
    placeholders: &[],
}];

/// A session id is `tok:`, `ssh:` or `agt:` and a short id.
pub(crate) fn id_ok(id: &str) -> Result<()> {
    let ok = ["tok:", "ssh:", "agt:"].iter().any(|p| {
        id.strip_prefix(p).is_some_and(|r| {
            !r.is_empty()
                && r.len() <= 64
                && r.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
    });
    if ok {
        Ok(())
    } else {
        Err(bad("sessions", "an id looks like tok:ID, ssh:ID or agt:ID"))
    }
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    match a.cmd {
        Sub::List(c) => {
            let conn = Conn::open(ctx, c.remote.as_deref()).await?;
            let msg = wire::ListSessionsRequest {
                login: c.login.clone().unwrap_or_default(),
            };
            let r = conn
                .call(false, |s, _| {
                    let m = msg.clone();
                    async move {
                        Ok(client(&s)
                            .list_sessions(s.wire.request(m, None))
                            .await?
                            .into_inner())
                    }
                })
                .await?;
            if c.json {
                return json("asc.v1.ListSessionsResponse", &r);
            }
            out(|out| {
                for x in &r.sessions {
                    writeln!(
                        out,
                        "{}  {}  {}  created {}  last used {}",
                        clean_str(&x.id),
                        clean_str(&x.label),
                        clean_str(&x.scope),
                        when(x.created),
                        when(x.last_used)
                    )?;
                }
                Ok(ExitCode::SUCCESS)
            })
        }
        Sub::Revoke(x) => {
            id_ok(&x.id)?;
            let conn = Conn::open(ctx, x.common.remote.as_deref()).await?;
            let msg = wire::RevokeSessionRequest {
                login: x.common.login.clone().unwrap_or_default(),
                id: x.id.clone(),
            };
            conn.call(true, |s, op| {
                let m = msg.clone();
                async move {
                    Ok(client(&s)
                        .revoke_session(s.wire.request(m, op))
                        .await?
                        .into_inner())
                }
            })
            .await?;
            warn_!("asc: {} revoked", clean_str(&x.id));
            Ok(ExitCode::SUCCESS)
        }
        Sub::Others(x) => {
            if let Some(k) = &x.keep {
                id_ok(k)?;
            }
            let conn = Conn::open(ctx, x.common.remote.as_deref()).await?;
            let msg = wire::RevokeOtherSessionsRequest {
                login: x.common.login.clone().unwrap_or_default(),
                keep_id: x.keep.clone().unwrap_or_default(),
            };
            let r = conn
                .call(true, |s, op| {
                    let m = msg.clone();
                    async move {
                        Ok(client(&s)
                            .revoke_other_sessions(s.wire.request(m, op))
                            .await?
                            .into_inner())
                    }
                })
                .await?;
            warn_!("asc: {} signed out", r.revoked);
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_session_ids_pass() {
        for ok in ["tok:abc123", "ssh:k_1", "agt:a-1"] {
            assert!(id_ok(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "tok:",
            "abc",
            "tok:../x",
            "tok:a b",
            "usr:1",
            "tok:\u{1b}[31m",
        ] {
            assert!(id_ok(bad).is_err(), "{bad:?}");
        }
    }
}
