//! `asc deps propose`.

use std::io::Write as _;
use std::process::ExitCode;

use asc_rpc::server::wire;

use super::{bad, client, kind_name, remote_with_token};
use crate::cli::agent::conn::Conn;
use crate::cli::codeintel::{clean_str, emit_json, lines, with_stdout};
use crate::cli::common::{Ctx, Result};

#[derive(clap::Args, Debug)]
pub struct ProposeArgs {
    /// Packages to update, `name` or `ecosystem:name`; every update the settings allow when none.
    packages: Vec<String>,
    /// Only updates that fix an open advisory.
    #[arg(long)]
    security: bool,
    /// The largest step a proposal makes: patch, minor or major. The repository's setting when absent.
    #[arg(long = "max-kind")]
    max_kind: Option<String>,
    /// Most changes one run records.
    #[arg(long = "max-groups")]
    max_groups: Option<u32>,
    /// Show the plan and record nothing.
    #[arg(long = "dry-run")]
    dry_run: bool,
    #[arg(long)]
    rev: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long = "token-env")]
    token_env: Option<String>,
    #[arg(long)]
    json: bool,
}

/// `name` or `ecosystem:name` as a selector. An npm scope (`@scope/name`) is a name, not an ecosystem.
pub(crate) fn selector(raw: &str) -> Result<wire::DepSelector> {
    let (eco, name) = match raw.split_once(':') {
        Some((e, n)) if !e.is_empty() && !e.contains('/') && !e.starts_with('@') => (e, n),
        _ => ("", raw),
    };
    if name.is_empty() || name.len() > 214 || name.chars().any(char::is_control) {
        return Err(bad(format!("{raw:?} is not a package")));
    }
    if eco.is_empty() {
        // The server matches on the ecosystem too; without one every ecosystem is tried.
        return Err(bad(format!(
            "{raw:?} needs an ecosystem: write it as ecosystem:name (for example npm:{name})"
        )));
    }
    super::ecosystems(&[eco.to_owned()])?;
    Ok(wire::DepSelector {
        ecosystem: eco.to_owned(),
        name: name.to_owned(),
    })
}

pub(crate) fn max_kind(s: &str) -> Result<i32> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "patch" => wire::DepUpdateKind::Patch,
        "minor" => wire::DepUpdateKind::Minor,
        "major" => wire::DepUpdateKind::Major,
        other => return Err(bad(format!("--max-kind is patch, minor or major, not {other:?}"))),
    } as i32)
}

pub async fn run(ctx: &Ctx, a: ProposeArgs) -> Result<ExitCode> {
    let remote = remote_with_token(a.remote.as_deref(), a.token_env.as_deref(), |k| {
        std::env::var(k).ok()
    })?;
    let selectors = a.packages.iter().map(|p| selector(p)).collect::<Result<Vec<_>>>()?;
    let kind = a.max_kind.as_deref().map(max_kind).transpose()?.unwrap_or(0);
    let conn = Conn::open(ctx, remote.as_deref()).await?;
    let repo = conn.repo_ref();
    let r = conn
        .call(!a.dry_run, |s, op| {
            let m = wire::ProposeDepUpdateRequest {
                repo: Some(repo.clone()),
                rev: a.rev.clone().unwrap_or_default(),
                selectors: selectors.clone(),
                security_only: a.security,
                max_kind: kind,
                max_groups: a.max_groups.unwrap_or(0),
                dry_run: a.dry_run,
            };
            async move {
                Ok(client(&s)
                    .propose_update(s.wire.request(m, op))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.ProposeDepUpdateResponse", &r)?;
        } else {
            for p in &r.proposals {
                writeln!(
                    out,
                    "{}\t{}\t{}",
                    if p.changeset.is_empty() { "plan" } else { "proposed" },
                    clean_str(&p.r#ref),
                    p.updates.len()
                )?;
                for u in &p.updates {
                    writeln!(
                        out,
                        "  {} {} {} -> {} ({})",
                        clean_str(&u.ecosystem),
                        clean_str(&u.name),
                        clean_str(&u.current),
                        clean_str(&u.target),
                        kind_name(u.kind)
                    )?;
                }
                for n in &p.notes {
                    writeln!(out, "  # {}", clean_str(n))?;
                }
            }
            for s in &r.skipped {
                writeln!(
                    out,
                    "skipped\t{} {}\t{}",
                    clean_str(&s.ecosystem),
                    clean_str(&s.name),
                    clean_str(&s.reason)
                )?;
            }
        }
        Ok(if r.proposals.is_empty() {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_package_is_named_with_its_ecosystem() {
        let s = selector("npm:@scope/pkg").unwrap();
        assert_eq!((s.ecosystem.as_str(), s.name.as_str()), ("npm", "@scope/pkg"));
        let s = selector("cargo:serde").unwrap();
        assert_eq!((s.ecosystem.as_str(), s.name.as_str()), ("cargo", "serde"));
        let s = selector("maven:org.apache.commons:commons-lang3").unwrap();
        assert_eq!(s.name, "org.apache.commons:commons-lang3");
        for bad in ["serde", "@scope/pkg", "cobol:x", "npm:", ":x", "npm:a\nb"] {
            assert!(selector(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_largest_step_is_patch_minor_or_major() {
        assert_eq!(max_kind("Minor").unwrap(), wire::DepUpdateKind::Minor as i32);
        assert!(max_kind("huge").is_err());
    }
}
