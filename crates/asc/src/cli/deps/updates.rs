//! `asc deps updates | config`.

use std::io::Write as _;
use std::process::ExitCode;

use asc_rpc::server::wire;

use super::{bad, client, ecosystems, kind_name, remote_with_token};
use crate::cli::agent::conn::{Conn, clean_text};
use crate::cli::codeintel::{clean_str, emit_json, lines, with_stdout};
use crate::cli::common::{Ctx, Result};

#[derive(clap::Args, Debug)]
pub struct UpdatesArgs {
    #[arg(long)]
    rev: Option<String>,
    #[arg(long = "ecosystem")]
    ecosystems: Vec<String>,
    /// Only updates that fix an open advisory.
    #[arg(long)]
    security: bool,
    /// Also the updates `.asc/deps.toml` tells the server to leave alone.
    #[arg(long)]
    ignored: bool,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long = "token-env")]
    token_env: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct ConfigArgs {
    #[arg(long)]
    rev: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long = "token-env")]
    token_env: Option<String>,
    #[arg(long)]
    json: bool,
}

/// One update as a line: package, current, target (latest), kind, what it fixes.
pub(crate) fn update_line(u: &wire::DepUpdate) -> String {
    let target = if u.target.is_empty() {
        "-".to_owned()
    } else {
        clean_str(&u.target)
    };
    let mut line = format!(
        "{} {}\t{} -> {}\t(latest {}, {})",
        clean_str(&u.ecosystem),
        clean_str(&u.name),
        clean_str(&u.current),
        target,
        if u.latest.is_empty() {
            "?".to_owned()
        } else {
            clean_str(&u.latest)
        },
        kind_name(u.kind)
    );
    if !u.fixes.is_empty() {
        line.push_str(&format!("\tfixes {}", clean_str(&u.fixes.join(", "))));
    }
    if !u.unfixed.is_empty() {
        line.push_str(&format!("\tstill affected by {}", clean_str(&u.unfixed.join(", "))));
    }
    if u.ignored {
        line.push_str("\tignored");
    }
    if !u.error.is_empty() {
        line.push_str(&format!("\t{}", clean_str(&u.error)));
    }
    line
}

pub async fn run(ctx: &Ctx, a: UpdatesArgs) -> Result<ExitCode> {
    let remote = remote_with_token(a.remote.as_deref(), a.token_env.as_deref(), |k| {
        std::env::var(k).ok()
    })?;
    let ecos = ecosystems(&a.ecosystems)?;
    let conn = Conn::open(ctx, remote.as_deref()).await?;
    let repo = conn.repo_ref();
    let r = conn
        .call(false, |s, _| {
            let m = wire::ListDepUpdatesRequest {
                repo: Some(repo.clone()),
                rev: a.rev.clone().unwrap_or_default(),
                ecosystems: ecos.clone(),
                security_only: a.security,
                include_ignored: a.ignored,
            };
            async move {
                Ok(client(&s)
                    .list_updates(s.wire.request(m, None))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.ListDepUpdatesResponse", &r)?;
        } else {
            for u in &r.updates {
                writeln!(out, "{}", update_line(u))?;
            }
            writeln!(
                out,
                "# {} packages, {} updates, {} could not be asked",
                r.packages,
                r.updates.len(),
                r.unknown_packages
            )?;
            for e in &r.lookup_errors {
                writeln!(out, "# {}", clean_str(e))?;
            }
        }
        Ok(if r.updates.is_empty() {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        })
    })
}

pub async fn config(ctx: &Ctx, a: ConfigArgs) -> Result<ExitCode> {
    let remote = remote_with_token(a.remote.as_deref(), a.token_env.as_deref(), |k| {
        std::env::var(k).ok()
    })?;
    if let Some(r) = &a.rev
        && (r.is_empty() || r.len() > 1024)
    {
        return Err(bad("--rev is a revision"));
    }
    let conn = Conn::open(ctx, remote.as_deref()).await?;
    let repo = conn.repo_ref();
    let r = conn
        .call(false, |s, _| {
            let m = wire::GetDepsConfigRequest {
                repo: Some(repo.clone()),
                rev: a.rev.clone().unwrap_or_default(),
            };
            async move {
                Ok(client(&s)
                    .get_config(s.wire.request(m, None))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.GetDepsConfigResponse", &r)?;
        } else {
            writeln!(out, "# source: {}", clean_str(&r.source))?;
            if !r.error.is_empty() {
                writeln!(out, "# the file cannot be used: {}", clean_str(&r.error))?;
            }
            if !r.text.is_empty() {
                writeln!(out, "{}", clean_text(&r.text))?;
            }
            if !r.workflow.is_empty() {
                writeln!(out, "# .asc/workflows/deps.toml for this schedule:")?;
                writeln!(out, "{}", clean_text(&r.workflow))?;
            }
        }
        Ok(ExitCode::SUCCESS)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_update_line_names_the_move_and_what_it_fixes() {
        let u = wire::DepUpdate {
            ecosystem: "crates.io".into(),
            name: "serde".into(),
            current: "1.0.100".into(),
            latest: "2.0.0".into(),
            target: "1.0.200".into(),
            kind: wire::DepUpdateKind::Major as i32,
            fixes: vec!["RUSTSEC-1".into()],
            unfixed: vec!["RUSTSEC-2".into()],
            ..Default::default()
        };
        assert_eq!(
            update_line(&u),
            "crates.io serde\t1.0.100 -> 1.0.200\t(latest 2.0.0, major)\tfixes RUSTSEC-1\tstill affected by RUSTSEC-2"
        );
        let blocked = wire::DepUpdate {
            target: String::new(),
            latest: String::new(),
            error: "the newest version is a major update".into(),
            ignored: true,
            ..u
        };
        let l = update_line(&blocked);
        assert!(l.contains("-> -\t(latest ?, major)") && l.ends_with("a major update"), "{l}");
        assert!(l.contains("\tignored"));
    }
}
