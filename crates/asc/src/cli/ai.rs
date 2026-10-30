//! `asc ai share <key> --with user:N|org:N|repo:O/N | shares <key> | unshare <key> <share> | caps <key>`:
//! sharing an AI key and the warning and stop rules of its cap (`AiKeyShares`). Keys themselves are
//! `asc agent key`.

use std::process::ExitCode;

use asc_rpc::server::wire;
use tonic::transport::Channel;

use super::acct::{bad, json};
use super::acct::{clean_str, out, say, warn_};
use super::agent::conn::{Conn, MAX_MESSAGE};
use super::common::{Ctx, Result};
use crate::native::NativeSpec;
use asc_rpc::services::ai_key_shares_client::AiKeySharesClient;

use super::acct::parse_owner;

fn client(s: &crate::remote::server::Session) -> AiKeySharesClient<Channel> {
    AiKeySharesClient::new(s.wire.channel.clone())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE)
}

#[derive(clap::Parser, Debug)]
#[command(
    name = "ai",
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
    Share(ShareArgs),
    Shares(KeyArgs),
    Unshare(UnshareArgs),
    Caps(CapsArgs),
}

#[derive(clap::Args, Debug)]
pub struct Common {
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct KeyArgs {
    key: String,
    #[command(flatten)]
    common: Common,
}

#[derive(clap::Args, Debug)]
pub struct ShareArgs {
    key: String,
    /// `user:NAME`, `org:NAME` or `repo:OWNER/NAME`.
    #[arg(long = "with")]
    with: String,
    /// The grantee may see the key's label but its agents may not use it.
    #[arg(long = "no-use")]
    no_use: bool,
    /// The grantee's administrators see what their own use cost.
    #[arg(long = "see-spend")]
    see_spend: bool,
    #[command(flatten)]
    common: Common,
}

#[derive(clap::Args, Debug)]
pub struct UnshareArgs {
    key: String,
    share: String,
    #[command(flatten)]
    common: Common,
}

#[derive(clap::Args, Debug)]
pub struct CapsArgs {
    key: String,
    /// Warn at this percentage of the monthly cap (0 = never).
    #[arg(long)]
    warn: Option<u32>,
    /// Stop calls at the cap (the default); `--no-stop` lets the key's own cap only warn.
    #[arg(long, conflicts_with = "no_stop")]
    stop: bool,
    #[arg(long = "no-stop")]
    no_stop: bool,
    #[command(flatten)]
    common: Common,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "ai",
    flags: &[
        "--remote",
        "--json",
        "--with",
        "--no-use",
        "--see-spend",
        "--warn",
        "--stop",
        "--no-stop",
    ],
    placeholders: &[],
}];

pub(crate) fn key_ok(id: &str) -> Result<()> {
    if id.starts_with("ak_")
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        Ok(())
    } else {
        Err(bad(
            "ai",
            "a key id looks like ak_ followed by letters and digits",
        ))
    }
}

pub(crate) fn caps_of(a: &CapsArgs, cur: &wire::AiKeyCaps) -> Result<(u32, bool)> {
    let warn = a.warn.unwrap_or(cur.warn_percent);
    if warn > 100 {
        return Err(bad("ai", "--warn is 0 to 100"));
    }
    let stop = if a.stop {
        true
    } else if a.no_stop {
        false
    } else {
        cur.stop_at_cap
    };
    Ok((warn, stop))
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    match a.cmd {
        Sub::Share(x) => {
            key_ok(&x.key)?;
            let grantee = parse_owner("ai", &x.with)?;
            if grantee.kind == wire::OwnerKind::Instance as i32 {
                return Err(bad(
                    "ai",
                    "a key is shared with a user, an organisation or a repository",
                ));
            }
            let conn = Conn::open(ctx, x.common.remote.as_deref()).await?;
            let msg = wire::ShareAiKeyRequest {
                key_id: x.key.clone(),
                grantee: Some(grantee),
                allow_use: !x.no_use,
                see_spend: x.see_spend,
            };
            let r = conn
                .call(true, |s, op| {
                    let m = msg.clone();
                    async move {
                        Ok(client(&s)
                            .share_key(s.wire.request(m, op))
                            .await?
                            .into_inner())
                    }
                })
                .await?;
            let sh = r.share.unwrap_or_default();
            if x.common.json {
                return json("asc.v1.AiKeyShare", &sh);
            }
            say!("{} shared with {}", clean_str(&sh.id), clean_str(&x.with));
            Ok(ExitCode::SUCCESS)
        }
        Sub::Shares(x) => {
            key_ok(&x.key)?;
            let conn = Conn::open(ctx, x.common.remote.as_deref()).await?;
            let msg = wire::ListAiKeySharesRequest {
                key_id: x.key.clone(),
            };
            let r = conn
                .call(false, |s, _| {
                    let m = msg.clone();
                    async move {
                        Ok(client(&s)
                            .list_shares(s.wire.request(m, None))
                            .await?
                            .into_inner())
                    }
                })
                .await?;
            if x.common.json {
                return json("asc.v1.ListAiKeySharesResponse", &r);
            }
            out(|out| {
                for s in &r.shares {
                    let g = s.grantee.clone().unwrap_or_default();
                    writeln!(
                        out,
                        "{}  {}  use={}  see-spend={}  by {}",
                        clean_str(&s.id),
                        clean_str(&g.name),
                        s.allow_use,
                        s.see_spend,
                        clean_str(&s.created_by)
                    )?;
                }
                Ok(ExitCode::SUCCESS)
            })
        }
        Sub::Unshare(x) => {
            key_ok(&x.key)?;
            let conn = Conn::open(ctx, x.common.remote.as_deref()).await?;
            let msg = wire::RevokeAiKeyShareRequest {
                key_id: x.key.clone(),
                share_id: x.share.clone(),
            };
            conn.call(true, |s, op| {
                let m = msg.clone();
                async move {
                    Ok(client(&s)
                        .revoke_share(s.wire.request(m, op))
                        .await?
                        .into_inner())
                }
            })
            .await?;
            warn_!("asc: share {} withdrawn", clean_str(&x.share));
            Ok(ExitCode::SUCCESS)
        }
        Sub::Caps(x) => {
            key_ok(&x.key)?;
            let conn = Conn::open(ctx, x.common.remote.as_deref()).await?;
            let get = wire::GetAiKeyCapsRequest {
                key_id: x.key.clone(),
            };
            let cur = conn
                .call(false, |s, _| {
                    let m = get.clone();
                    async move {
                        Ok(client(&s)
                            .get_key_caps(s.wire.request(m, None))
                            .await?
                            .into_inner()
                            .caps
                            .unwrap_or_default())
                    }
                })
                .await?;
            let caps = if x.warn.is_some() || x.stop || x.no_stop {
                let (warn, stop) = caps_of(&x, &cur)?;
                let msg = wire::SetAiKeyCapsRequest {
                    key_id: x.key.clone(),
                    warn_percent: warn,
                    stop_at_cap: stop,
                };
                conn.call(true, |s, op| {
                    let m = msg.clone();
                    async move {
                        Ok(client(&s)
                            .set_key_caps(s.wire.request(m, op))
                            .await?
                            .into_inner()
                            .caps
                            .unwrap_or_default())
                    }
                })
                .await?
            } else {
                cur
            };
            if x.common.json {
                return json("asc.v1.AiKeyCaps", &caps);
            }
            say!(
                "warn at {}%  stop at cap: {}  cap {} micro-USD  spent {}{}",
                caps.warn_percent,
                caps.stop_at_cap,
                caps.monthly_cap_micro_usd,
                caps.month_spent_micro_usd,
                if caps.warned {
                    "  (warned this month)"
                } else {
                    ""
                }
            );
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(extra: &[&str]) -> CapsArgs {
        let mut v = vec!["ai", "caps", "ak_abc"];
        v.extend_from_slice(extra);
        let Sub::Caps(c) = <Args as clap::Parser>::try_parse_from(v).unwrap().cmd else {
            unreachable!()
        };
        c
    }

    #[test]
    fn caps_flags_change_only_what_they_name() {
        let cur = wire::AiKeyCaps {
            warn_percent: 80,
            stop_at_cap: true,
            ..Default::default()
        };
        assert_eq!(caps_of(&caps(&["--warn", "50"]), &cur).unwrap(), (50, true));
        assert_eq!(caps_of(&caps(&["--no-stop"]), &cur).unwrap(), (80, false));
        assert!(caps_of(&caps(&["--warn", "101"]), &cur).is_err());
        assert!(
            <Args as clap::Parser>::try_parse_from(["ai", "caps", "ak_a", "--stop", "--no-stop"])
                .is_err()
        );
    }

    #[test]
    fn key_ids_are_checked_before_any_call() {
        assert!(key_ok("ak_abc123").is_ok());
        for bad in ["", "ak_a b", "x_abc", "ak_../x"] {
            assert!(key_ok(bad).is_err(), "{bad:?}");
        }
    }
}
