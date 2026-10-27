//! `asc agent key add | list | show | test | update | rm | models | usage`: the AI provider keys
//! an owner brings (`AiProviders`).
//!
//! A key is read from a file, an environment variable or standard input and sent once; the server
//! seals it and never returns it, so nothing here can print one. A key is scoped to an owner: a
//! user, an organisation or a repository (`--owner user:NAME`, `org:NAME`, `repo:OWNER/NAME`); with
//! no `--owner` it is the repository the remote names.

use std::process::ExitCode;

use asc_rpc::server::wire;
use asc_rpc::services::ai_providers_client::AiProvidersClient;
use tonic::transport::Channel;

use super::bad;
use super::conn::{Conn, MAX_MESSAGE, dollars, parse_dollars, short, when};
use crate::cli::codeintel::{clean_str, emit_json, lines, with_stdout};
use crate::cli::common::Result;
use crate::remote::server::Session;

fn client(s: &Session) -> AiProvidersClient<Channel> {
    AiProvidersClient::new(s.wire.channel.clone())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE)
}

#[derive(clap::Args, Debug)]
pub struct KeyArgs {
    #[command(subcommand)]
    cmd: KeySub,
}

#[derive(clap::Subcommand, Debug)]
enum KeySub {
    /// Stores a provider key; the key itself comes from a file, a variable or standard input.
    Add(AddArgs),
    /// Lists the keys of an owner.
    List(ListArgs),
    /// Shows one key (never its value).
    Show(IdArgs),
    /// Makes one tiny request to the provider with the key; a key is used only after it passed.
    Test(IdArgs),
    /// Changes a key's label, value, address, model, cap or whether it is disabled.
    Update(UpdateArgs),
    /// Removes a key.
    Rm(IdArgs),
    /// Lists the models a tested key can use.
    Models(IdArgs),
    /// Shows what the keys of an owner spent.
    Usage(UsageArgs),
}

#[derive(clap::Args, Debug)]
struct AddArgs {
    /// `anthropic`, `openai`, `google` or `openai-compatible` (which needs --base-url).
    #[arg(long)]
    provider: String,
    #[arg(long)]
    label: String,
    #[arg(long = "base-url")]
    base_url: Option<String>,
    #[arg(long)]
    model: Option<String>,
    /// Most the key may spend in a month, in dollars (`50`); the instance cap still holds.
    #[arg(long = "monthly-cap")]
    monthly_cap: Option<String>,
    /// `user:NAME`, `org:NAME`, `repo:OWNER/NAME` or `instance`; the remote's repository when absent.
    #[arg(long)]
    owner: Option<String>,
    /// Read the key from this file.
    #[arg(long = "key-file")]
    key_file: Option<String>,
    /// Read the key from this environment variable.
    #[arg(long = "key-env")]
    key_env: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
struct ListArgs {
    #[arg(long)]
    owner: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
struct IdArgs {
    id: String,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
struct UpdateArgs {
    id: String,
    #[arg(long)]
    label: Option<String>,
    #[arg(long = "base-url")]
    base_url: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long = "monthly-cap")]
    monthly_cap: Option<String>,
    /// Replace the key; the new value comes from --key-file, --key-env or standard input.
    #[arg(long = "new-key")]
    new_key: bool,
    #[arg(long = "key-file")]
    key_file: Option<String>,
    #[arg(long = "key-env")]
    key_env: Option<String>,
    /// Stop agents and workflows from using the key.
    #[arg(long, conflicts_with = "enable")]
    disable: bool,
    #[arg(long)]
    enable: bool,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
struct UsageArgs {
    #[arg(long)]
    owner: Option<String>,
    /// Only this key.
    #[arg(long)]
    key: Option<String>,
    /// `day`, `model`, `repo` or `agent`.
    #[arg(long = "group-by", default_value = "day")]
    group_by: String,
    /// Unix seconds or `YYYY-MM-DD`; this month so far when both are absent.
    #[arg(long)]
    since: Option<String>,
    #[arg(long)]
    until: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

pub(crate) fn provider_wire(name: &str) -> Result<i32> {
    use wire::AiProvider as P;
    Ok(match name.to_ascii_lowercase().replace('_', "-").as_str() {
        "anthropic" => P::Anthropic,
        "openai" => P::Openai,
        "google" | "gemini" => P::Google,
        "openai-compatible" | "compatible" => P::OpenaiCompatible,
        other => {
            return Err(bad(format!(
                "unknown provider {other:?}; use anthropic, openai, google or openai-compatible"
            )));
        }
    } as i32)
}

fn provider_name(n: i32) -> String {
    wire::AiProvider::try_from(n).map_or_else(
        |_| "?".to_owned(),
        |p| short(p.as_str_name(), "AI_PROVIDER_").replace('_', "-"),
    )
}

fn key_state(n: i32) -> String {
    wire::AiKeyState::try_from(n).map_or_else(
        |_| "?".to_owned(),
        |s| short(s.as_str_name(), "AI_KEY_STATE_"),
    )
}

/// An owner from `--owner`, or the repository of the connection.
pub(crate) fn owner_of(spec: Option<&str>, conn: &Conn) -> Result<wire::OwnerRef> {
    let Some(spec) = spec else {
        return Ok(wire::OwnerRef {
            kind: wire::OwnerKind::Repo as i32,
            name: conn.repo(),
        });
    };
    let (kind, name) = match spec.split_once(':') {
        Some(("user", n)) => (wire::OwnerKind::User, n),
        Some(("org", n)) => (wire::OwnerKind::Org, n),
        Some(("repo", n)) => (wire::OwnerKind::Repo, n),
        None if spec == "instance" => (wire::OwnerKind::Instance, ""),
        _ => {
            return Err(bad(
                "--owner is user:NAME, org:NAME, repo:OWNER/NAME or instance",
            ));
        }
    };
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/');
    let ok = match kind {
        wire::OwnerKind::Instance => name.is_empty(),
        wire::OwnerKind::Repo => name.split('/').count() == 2 && name.chars().all(plain),
        _ => {
            !name.is_empty() && name.len() <= 100 && name.chars().all(plain) && !name.contains('/')
        }
    };
    if !ok {
        return Err(bad(format!("{spec:?} is not a valid owner")));
    }
    Ok(wire::OwnerRef {
        kind: kind as i32,
        name: name.to_owned(),
    })
}

fn cap_micro(s: &Option<String>) -> Result<Option<u64>> {
    s.as_deref()
        .map(|v| {
            parse_dollars(v)
                .ok_or_else(|| bad("--monthly-cap is an amount in dollars, like 50 (0 for none)"))
        })
        .transpose()
}

fn key_block(out: &mut dyn std::io::Write, k: &wire::AiKey) -> std::io::Result<()> {
    writeln!(out, "key        {}", clean_str(&k.id))?;
    writeln!(out, "label      {}", clean_str(&k.label))?;
    writeln!(out, "provider   {}", provider_name(k.provider))?;
    if !k.base_url.is_empty() {
        writeln!(out, "address    {}", clean_str(&k.base_url))?;
    }
    if !k.default_model.is_empty() {
        writeln!(out, "model      {}", clean_str(&k.default_model))?;
    }
    writeln!(out, "state      {}", key_state(k.state))?;
    writeln!(
        out,
        "value      {}",
        if k.has_key {
            format!("stored, ends in {}", clean_str(&k.key_hint))
        } else {
            "none".to_owned()
        }
    )?;
    writeln!(
        out,
        "month      {} of {}",
        dollars(k.month_spent_micro_usd),
        if k.monthly_cap_micro_usd == 0 {
            "no cap".to_owned()
        } else {
            dollars(k.monthly_cap_micro_usd)
        }
    )?;
    writeln!(out, "tested     {}", when(k.tested_at))?;
    if !k.last_error.is_empty() {
        writeln!(out, "last error {}", clean_str(&k.last_error))?;
    }
    Ok(())
}

fn key_line(k: &wire::AiKey) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}",
        clean_str(&k.id),
        provider_name(k.provider),
        key_state(k.state),
        clean_str(&k.label),
        if k.has_key {
            format!("...{}", clean_str(&k.key_hint))
        } else {
            "-".to_owned()
        },
        dollars(k.month_spent_micro_usd)
    )
}

fn print_key(a_json: bool, k: &wire::AiKey) -> Result<ExitCode> {
    with_stdout(|out| {
        if a_json {
            emit_json(&mut lines(out), "asc.v1.AiKey", k)?;
        } else {
            key_block(out, k)?;
        }
        Ok(ExitCode::SUCCESS)
    })
}

pub async fn run(ctx: &crate::cli::common::Ctx, a: KeyArgs) -> Result<ExitCode> {
    match a.cmd {
        KeySub::Add(x) => add(ctx, x).await,
        KeySub::List(x) => list(ctx, x).await,
        KeySub::Show(x) => show(ctx, x).await,
        KeySub::Test(x) => test(ctx, x).await,
        KeySub::Update(x) => update(ctx, x).await,
        KeySub::Rm(x) => rm(ctx, x).await,
        KeySub::Models(x) => models(ctx, x).await,
        KeySub::Usage(x) => usage(ctx, x).await,
    }
}

async fn add(ctx: &crate::cli::common::Ctx, a: AddArgs) -> Result<ExitCode> {
    let provider = provider_wire(&a.provider)?;
    if provider == wire::AiProvider::OpenaiCompatible as i32 && a.base_url.is_none() {
        return Err(bad("an openai-compatible provider needs --base-url"));
    }
    if a.label.trim().is_empty() || a.label.len() > 64 {
        return Err(bad("--label is 1 to 64 bytes"));
    }
    let cap = cap_micro(&a.monthly_cap)?;
    let api_key = super::read_secret(a.key_file.as_deref(), a.key_env.as_deref(), "API key")?;
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let msg = wire::CreateAiKeyRequest {
        owner: Some(owner_of(a.owner.as_deref(), &conn)?),
        label: a.label.trim().to_owned(),
        provider,
        api_key,
        base_url: a.base_url.clone().unwrap_or_default(),
        default_model: a.model.clone().unwrap_or_default(),
        monthly_cap_micro_usd: cap.unwrap_or(0),
    };
    let key = conn
        .call(true, |s, op| {
            let m = msg.clone();
            async move {
                Ok(client(&s)
                    .create_key(s.wire.request(m, op))
                    .await?
                    .into_inner()
                    .key
                    .unwrap_or_default())
            }
        })
        .await?;
    eprintln!(
        "asc: key {} stored; test it with `asc agent key test {}` before agents use it",
        clean_str(&key.id),
        clean_str(&key.id)
    );
    print_key(a.json, &key)
}

async fn list(ctx: &crate::cli::common::Ctx, a: ListArgs) -> Result<ExitCode> {
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let msg = wire::ListAiKeysRequest {
        owner: Some(owner_of(a.owner.as_deref(), &conn)?),
    };
    let r = conn
        .call(false, |s, _| {
            let m = msg.clone();
            async move {
                Ok(client(&s)
                    .list_keys(s.wire.request(m, None))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.ListAiKeysResponse", &r)?;
        } else {
            for k in &r.keys {
                writeln!(out, "{}", key_line(k))?;
            }
        }
        Ok(if r.keys.is_empty() {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        })
    })
}

fn id_ok(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 64 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(bad(
            "a key id looks like ak_ followed by letters and digits",
        ));
    }
    Ok(())
}

async fn show(ctx: &crate::cli::common::Ctx, a: IdArgs) -> Result<ExitCode> {
    id_ok(&a.id)?;
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let id = a.id.clone();
    let key = conn
        .call(false, |s, _| {
            let m = wire::GetAiKeyRequest { id: id.clone() };
            async move {
                Ok(client(&s)
                    .get_key(s.wire.request(m, None))
                    .await?
                    .into_inner()
                    .key
                    .unwrap_or_default())
            }
        })
        .await?;
    print_key(a.json, &key)
}

async fn test(ctx: &crate::cli::common::Ctx, a: IdArgs) -> Result<ExitCode> {
    id_ok(&a.id)?;
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let id = a.id.clone();
    let r = conn
        .call(false, |s, _| {
            let m = wire::TestAiKeyRequest { id: id.clone() };
            async move {
                Ok(client(&s)
                    .test_key(s.wire.request(m, None))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.TestAiKeyResponse", &r)?;
        } else {
            writeln!(
                out,
                "{}\t{}ms\t{}",
                clean_str(&r.provider_status),
                r.latency_ms,
                clean_str(&r.message)
            )?;
        }
        Ok(if r.ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        })
    })
}

async fn update(ctx: &crate::cli::common::Ctx, a: UpdateArgs) -> Result<ExitCode> {
    id_ok(&a.id)?;
    let cap = cap_micro(&a.monthly_cap)?;
    let api_key = if a.new_key {
        Some(super::read_secret(
            a.key_file.as_deref(),
            a.key_env.as_deref(),
            "API key",
        )?)
    } else if a.key_file.is_some() || a.key_env.is_some() {
        return Err(bad("--key-file and --key-env need --new-key"));
    } else {
        None
    };
    let disabled = if a.disable {
        Some(true)
    } else if a.enable {
        Some(false)
    } else {
        None
    };
    if a.label.is_none()
        && a.base_url.is_none()
        && a.model.is_none()
        && cap.is_none()
        && api_key.is_none()
        && disabled.is_none()
    {
        return Err(bad(
            "name what to change: --label, --base-url, --model, --monthly-cap, --new-key, --disable or --enable",
        ));
    }
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let msg = wire::UpdateAiKeyRequest {
        id: a.id.clone(),
        label: a.label.clone(),
        api_key,
        base_url: a.base_url.clone(),
        default_model: a.model.clone(),
        monthly_cap_micro_usd: cap,
        disabled,
    };
    let key = conn
        .call(true, |s, op| {
            let m = msg.clone();
            async move {
                Ok(client(&s)
                    .update_key(s.wire.request(m, op))
                    .await?
                    .into_inner()
                    .key
                    .unwrap_or_default())
            }
        })
        .await?;
    print_key(a.json, &key)
}

async fn rm(ctx: &crate::cli::common::Ctx, a: IdArgs) -> Result<ExitCode> {
    id_ok(&a.id)?;
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let id = a.id.clone();
    conn.call(false, |s, _| {
        let m = wire::DeleteAiKeyRequest { id: id.clone() };
        async move {
            client(&s)
                .delete_key(s.wire.request(m, None))
                .await
                .map(|_| ())
        }
    })
    .await?;
    if a.json {
        println!("{{}}");
    } else {
        eprintln!("asc: key {} removed", clean_str(&a.id));
    }
    Ok(ExitCode::SUCCESS)
}

async fn models(ctx: &crate::cli::common::Ctx, a: IdArgs) -> Result<ExitCode> {
    id_ok(&a.id)?;
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let id = a.id.clone();
    let r = conn
        .call(false, |s, _| {
            let m = wire::ListAiModelsRequest { id: id.clone() };
            async move {
                Ok(client(&s)
                    .list_models(s.wire.request(m, None))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.ListAiModelsResponse", &r)?;
        } else {
            for m in &r.models {
                writeln!(
                    out,
                    "{}\t{}\t{}",
                    clean_str(&m.id),
                    m.context_tokens,
                    clean_str(&m.display_name)
                )?;
            }
        }
        Ok(if r.models.is_empty() {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        })
    })
}

/// A moment from `--since` and `--until`: unix seconds or a UTC day.
fn moment(s: &str) -> Result<i64> {
    if let Ok(n) = s.parse::<i64>()
        && n >= 0
    {
        return Ok(n);
    }
    let p: Vec<&str> = s.split('-').collect();
    if let [y, m, d] = p.as_slice()
        && let (Ok(y), Ok(m), Ok(d)) = (y.parse::<i64>(), m.parse::<i64>(), d.parse::<i64>())
        && (1970..=9999).contains(&y)
        && (1..=12).contains(&m)
        && (1..=31).contains(&d)
    {
        // days from civil (Howard Hinnant)
        let y2 = if m <= 2 { y - 1 } else { y };
        let era = y2.div_euclid(400);
        let yoe = y2.rem_euclid(400);
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        return Ok((era * 146_097 + doe - 719_468) * 86_400);
    }
    Err(bad(format!(
        "{s:?} is not unix seconds or a day like 2026-10-01"
    )))
}

async fn usage(ctx: &crate::cli::common::Ctx, a: UsageArgs) -> Result<ExitCode> {
    use wire::AiUsageGroup as G;
    let group_by = match a.group_by.as_str() {
        "day" => G::Day,
        "model" => G::Model,
        "repo" => G::Repo,
        "agent" => G::Agent,
        other => {
            return Err(bad(format!(
                "--group-by is day, model, repo or agent, not {other:?}"
            )));
        }
    } as i32;
    let since = a.since.as_deref().map(moment).transpose()?.unwrap_or(0);
    let until = a.until.as_deref().map(moment).transpose()?.unwrap_or(0);
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let msg = wire::AiUsageRequest {
        owner: Some(owner_of(a.owner.as_deref(), &conn)?),
        key_id: a.key.clone().unwrap_or_default(),
        since,
        until,
        group_by,
    };
    let r = conn
        .call(false, |s, _| {
            let m = msg.clone();
            async move {
                Ok(client(&s)
                    .usage(s.wire.request(m, None))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.AiUsageResponse", &r)?;
        } else {
            let row = |x: &wire::AiUsageRow| {
                let unpriced = if x.unpriced_requests > 0 {
                    format!("\t{} unpriced", x.unpriced_requests)
                } else {
                    String::new()
                };
                format!(
                    "{}\t{} requests\t{} in\t{} out\t{}{unpriced}",
                    clean_str(&x.group),
                    x.requests,
                    x.input_tokens,
                    x.output_tokens,
                    dollars(x.cost_micro_usd)
                )
            };
            for x in &r.rows {
                writeln!(out, "{}", row(x))?;
            }
            if let Some(t) = &r.total {
                writeln!(
                    out,
                    "total\t{}",
                    row(t).split_once('\t').map_or("", |(_, rest)| rest)
                )?;
            }
        }
        Ok(ExitCode::SUCCESS)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[derive(clap::Parser, Debug)]
    #[command(
        name = "agent",
        disable_help_flag = true,
        disable_version_flag = true,
        disable_help_subcommand = true
    )]
    struct Probe {
        #[command(subcommand)]
        cmd: super::super::Sub,
    }

    fn parse(args: &[&str]) -> std::result::Result<KeyArgs, clap::Error> {
        let mut argv = vec!["agent", "key"];
        argv.extend_from_slice(args);
        Probe::try_parse_from(argv).map(|p| match p.cmd {
            super::super::Sub::Key(k) => k,
            _ => unreachable!("key"),
        })
    }

    #[test]
    fn a_key_is_never_an_argument() {
        // There is no flag that takes the key itself: only a file, a variable or standard input.
        assert!(
            parse(&[
                "add",
                "--provider",
                "openai",
                "--label",
                "x",
                "--api-key",
                "sk-nope"
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "add",
                "--provider",
                "openai",
                "--label",
                "x",
                "--key",
                "sk-nope"
            ])
            .is_err()
        );
        let a = parse(&[
            "add",
            "--provider",
            "anthropic",
            "--label",
            "team",
            "--key-env",
            "MY_KEY",
            "--monthly-cap",
            "25",
        ])
        .unwrap();
        let KeySub::Add(a) = a.cmd else { panic!("add") };
        assert_eq!(
            (a.key_env.as_deref(), a.key_file.as_deref()),
            (Some("MY_KEY"), None)
        );
        assert!(parse(&["update", "ak_1", "--disable", "--enable"]).is_err());
    }

    #[test]
    fn providers_amounts_and_days_parse_exactly() {
        assert_eq!(
            provider_wire("Anthropic").unwrap(),
            wire::AiProvider::Anthropic as i32
        );
        assert_eq!(
            provider_wire("openai_compatible").unwrap(),
            wire::AiProvider::OpenaiCompatible as i32
        );
        assert!(provider_wire("bard").is_err());
        assert_eq!(
            provider_name(wire::AiProvider::OpenaiCompatible as i32),
            "openai-compatible"
        );
        assert_eq!(cap_micro(&Some("25".into())).unwrap(), Some(25_000_000));
        assert!(cap_micro(&Some("-1".into())).is_err());
        assert_eq!(moment("0").unwrap(), 0);
        assert_eq!(moment("1700000000").unwrap(), 1_700_000_000);
        assert_eq!(moment("2023-11-14").unwrap(), 1_699_920_000);
        assert_eq!(moment("1970-01-02").unwrap(), 86_400);
        for bad in ["", "yesterday", "2023-13-01", "2023-11", "-5"] {
            assert!(moment(bad).is_err(), "{bad:?}");
        }
        assert!(id_ok("ak_Abc123").is_ok());
        for bad in ["", "ak 1", "../x", &"a".repeat(65)] {
            assert!(id_ok(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_key_is_shown_by_its_last_characters_and_as_clean_text() {
        let k = wire::AiKey {
            id: "ak_1".into(),
            label: "te\u{1b}[31mam".into(),
            provider: wire::AiProvider::Anthropic as i32,
            state: wire::AiKeyState::Active as i32,
            key_hint: "wxyz".into(),
            has_key: true,
            monthly_cap_micro_usd: 25_000_000,
            month_spent_micro_usd: 1_250_000,
            ..Default::default()
        };
        let mut out = Vec::new();
        key_block(&mut out, &k).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("stored, ends in wxyz") && text.contains("$1.25 of $25"),
            "{text}"
        );
        assert!(!text.contains('\u{1b}'));
        let line = key_line(&k);
        assert!(
            line.contains("anthropic") && line.contains("active") && line.contains("...wxyz"),
            "{line}"
        );
    }
}
