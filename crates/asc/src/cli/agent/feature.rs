//! `asc agent feature list | set | clear`: which AI key a feature uses and the monthly cap it
//! spends under, for an owner (`AiProviders.ListFeatures`, `AiProviders.SetFeature`).
//!
//! The most specific owner of the chain (repository, its organisation or user, the instance) with
//! a setting chooses the key; every cap of the chain holds.

use std::process::ExitCode;

use asc_rpc::server::wire;
use asc_rpc::services::ai_providers_client::AiProvidersClient;
use tonic::transport::Channel;

use super::bad;
use super::conn::{Conn, MAX_MESSAGE, dollars, short, when};
use super::key::{cap_micro, id_ok, owner_of};
use crate::cli::codeintel::{clean_str, emit_json, lines, with_stdout};
use crate::cli::common::Result;
use crate::remote::server::Session;

fn client(s: &Session) -> AiProvidersClient<Channel> {
    AiProvidersClient::new(s.wire.channel.clone())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE)
}

#[derive(clap::Args, Debug)]
pub struct FeatureArgs {
    #[command(subcommand)]
    cmd: FeatureSub,
}

#[derive(clap::Subcommand, Debug)]
enum FeatureSub {
    /// Lists every AI feature with the key it uses, its cap and this month's spending.
    List(ListArgs),
    /// Chooses the key and the monthly cap of a feature for the owner.
    Set(SetArgs),
    /// Removes the owner's setting of a feature, so the setting of a less specific owner applies.
    Clear(ClearArgs),
}

#[derive(clap::Args, Debug)]
struct ListArgs {
    /// user:NAME, org:NAME, repo:OWNER/NAME or instance; default the repository of the remote.
    #[arg(long)]
    owner: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
struct SetArgs {
    /// agent-runs or review-summaries.
    feature: String,
    /// The key to use; default the first usable key of the chain.
    #[arg(long)]
    key: Option<String>,
    /// The most the feature spends in a month, in dollars (0 for no cap).
    #[arg(long = "monthly-cap")]
    monthly_cap: Option<String>,
    #[arg(long)]
    owner: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
struct ClearArgs {
    feature: String,
    #[arg(long)]
    owner: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

pub async fn run(ctx: &crate::cli::common::Ctx, a: FeatureArgs) -> Result<ExitCode> {
    match a.cmd {
        FeatureSub::List(x) => list(ctx, x).await,
        FeatureSub::Set(x) => set(ctx, x).await,
        FeatureSub::Clear(x) => clear(ctx, x).await,
    }
}

fn feature_wire(name: &str) -> Result<i32> {
    use wire::AiFeature as F;
    Ok(match name.to_ascii_lowercase().replace('_', "-").as_str() {
        "agent-runs" => F::AgentRuns,
        "review-summaries" => F::ReviewSummaries,
        other => {
            return Err(bad(format!(
                "unknown feature {other:?}; use agent-runs or review-summaries"
            )));
        }
    } as i32)
}

fn feature_name(n: i32) -> String {
    wire::AiFeature::try_from(n).map_or_else(
        |_| "?".to_owned(),
        |f| short(f.as_str_name(), "AI_FEATURE_").replace('_', "-"),
    )
}

fn owner_text(o: Option<&wire::OwnerRef>) -> String {
    let Some(o) = o else {
        return "-".to_owned();
    };
    match wire::OwnerKind::try_from(o.kind) {
        Ok(wire::OwnerKind::Instance) => "instance".to_owned(),
        Ok(wire::OwnerKind::User) => format!("user:{}", clean_str(&o.name)),
        Ok(wire::OwnerKind::Org) => format!("org:{}", clean_str(&o.name)),
        Ok(wire::OwnerKind::Repo) => format!("repo:{}", clean_str(&o.name)),
        _ => "-".to_owned(),
    }
}

fn cap_text(micro: u64) -> String {
    if micro == 0 {
        "no cap".to_owned()
    } else {
        dollars(micro)
    }
}

fn feature_line(f: &wire::AiFeatureSetting) -> String {
    format!(
        "{}\t{}\t{}\t{} of {}\t{}",
        feature_name(f.feature),
        owner_text(f.set_by.as_ref()),
        f.key
            .as_ref()
            .map_or_else(|| "-".to_owned(), |k| clean_str(&k.id)),
        dollars(f.month_spent_micro_usd),
        cap_text(f.monthly_cap_micro_usd),
        if f.paused {
            format!("paused until {}", when(f.resumes_at))
        } else {
            "on".to_owned()
        }
    )
}

fn print_feature(json: bool, f: &wire::AiFeatureSetting) -> Result<ExitCode> {
    with_stdout(|out| {
        if json {
            emit_json(&mut lines(out), "asc.v1.AiFeatureSetting", f)?;
            return Ok(ExitCode::SUCCESS);
        }
        writeln!(out, "feature    {}", feature_name(f.feature))?;
        writeln!(out, "set by     {}", owner_text(f.set_by.as_ref()))?;
        writeln!(
            out,
            "key        {}",
            match &f.key {
                Some(k) => format!("{} ({})", clean_str(&k.id), clean_str(&k.label)),
                None if f.key_id.is_empty() => "the first usable key".to_owned(),
                None => format!("{} (not usable)", clean_str(&f.key_id)),
            }
        )?;
        writeln!(
            out,
            "month      {} of {}",
            dollars(f.month_spent_micro_usd),
            cap_text(f.monthly_cap_micro_usd)
        )?;
        if f.paused {
            writeln!(
                out,
                "paused     by {} until {}",
                owner_text(f.paused_by.as_ref()),
                when(f.resumes_at)
            )?;
        }
        if f.updated > 0 {
            writeln!(
                out,
                "updated    {} by {}",
                when(f.updated),
                clean_str(&f.updated_by)
            )?;
        }
        Ok(ExitCode::SUCCESS)
    })
}

async fn list(ctx: &crate::cli::common::Ctx, a: ListArgs) -> Result<ExitCode> {
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let msg = wire::ListAiFeaturesRequest {
        owner: Some(owner_of(a.owner.as_deref(), &conn)?),
    };
    let r = conn
        .call(false, |s, _| {
            let m = msg.clone();
            async move {
                Ok(client(&s)
                    .list_features(s.wire.request(m, None))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.ListAiFeaturesResponse", &r)?;
        } else {
            for f in &r.features {
                writeln!(out, "{}", feature_line(f))?;
            }
        }
        Ok(if r.features.is_empty() {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        })
    })
}

async fn send(
    ctx: &crate::cli::common::Ctx,
    remote: Option<&str>,
    owner: Option<&str>,
    json: bool,
    build: impl FnOnce(wire::OwnerRef) -> wire::SetAiFeatureRequest,
) -> Result<ExitCode> {
    let conn = Conn::open(ctx, remote).await?;
    let msg = build(owner_of(owner, &conn)?);
    let f = conn
        .call(true, |s, op| {
            let m = msg.clone();
            async move {
                Ok(client(&s)
                    .set_feature(s.wire.request(m, op))
                    .await?
                    .into_inner()
                    .feature
                    .unwrap_or_default())
            }
        })
        .await?;
    print_feature(json, &f)
}

async fn set(ctx: &crate::cli::common::Ctx, a: SetArgs) -> Result<ExitCode> {
    let feature = feature_wire(&a.feature)?;
    if let Some(k) = &a.key {
        id_ok(k)?;
    }
    let cap = cap_micro(&a.monthly_cap)?.unwrap_or(0);
    let key_id = a.key.clone().unwrap_or_default();
    send(
        ctx,
        a.remote.as_deref(),
        a.owner.as_deref(),
        a.json,
        |owner| wire::SetAiFeatureRequest {
            owner: Some(owner),
            feature,
            key_id,
            monthly_cap_micro_usd: cap,
            clear: false,
        },
    )
    .await
}

async fn clear(ctx: &crate::cli::common::Ctx, a: ClearArgs) -> Result<ExitCode> {
    let feature = feature_wire(&a.feature)?;
    send(
        ctx,
        a.remote.as_deref(),
        a.owner.as_deref(),
        a.json,
        |owner| wire::SetAiFeatureRequest {
            owner: Some(owner),
            feature,
            clear: true,
            ..Default::default()
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_names_round_trip_and_an_unknown_one_is_refused() {
        for name in ["agent-runs", "review-summaries"] {
            assert_eq!(feature_name(feature_wire(name).unwrap()), name);
        }
        assert_eq!(
            feature_wire("REVIEW_SUMMARIES").unwrap(),
            wire::AiFeature::ReviewSummaries as i32
        );
        assert!(feature_wire("meaning-search").is_err());
        assert_eq!(feature_name(99), "?");
    }

    #[test]
    fn an_owner_prints_the_way_owner_is_written() {
        let o = |kind: wire::OwnerKind, name: &str| wire::OwnerRef {
            kind: kind as i32,
            name: name.to_owned(),
        };
        assert_eq!(owner_text(None), "-");
        assert_eq!(
            owner_text(Some(&o(wire::OwnerKind::Instance, ""))),
            "instance"
        );
        assert_eq!(
            owner_text(Some(&o(wire::OwnerKind::Org, "acme"))),
            "org:acme"
        );
        assert_eq!(
            owner_text(Some(&o(wire::OwnerKind::Repo, "acme/nova"))),
            "repo:acme/nova"
        );
    }
}
