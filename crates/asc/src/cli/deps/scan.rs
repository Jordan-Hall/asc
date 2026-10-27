//! `asc deps scan`.

use std::io::Write as _;
use std::process::ExitCode;

use asc_rpc::server::wire;

use super::{bad, client, ecosystems, lookup_name, remote_with_token, severity_name};
use crate::cli::agent::conn::Conn;
use crate::cli::codeintel::{clean_str, emit_json, lines, with_stdout};
use crate::cli::common::{Ctx, Result};

#[derive(clap::Args, Debug)]
pub struct ScanArgs {
    /// The revision to read; trunk when absent. Only a scan of trunk is recorded.
    #[arg(long)]
    rev: Option<String>,
    /// Only this ecosystem (OSV name or alias); repeat.
    #[arg(long = "ecosystem")]
    ecosystems: Vec<String>,
    /// Ask the advisory source again instead of using what it said lately.
    #[arg(long)]
    refresh: bool,
    #[arg(long)]
    remote: Option<String>,
    /// Take the credential from this environment variable (needs --remote <https URL>).
    #[arg(long = "token-env")]
    token_env: Option<String>,
    #[arg(long)]
    json: bool,
}

/// One finding as a line: severity, package, version, advisory, fix, file.
pub(crate) fn finding_line(f: &wire::DepFinding) -> String {
    let a = f.advisory.clone().unwrap_or_default();
    format!(
        "{}\t{} {} {}\t{}\t{}\t{}{}",
        severity_name(a.severity),
        clean_str(&f.ecosystem),
        clean_str(&f.name),
        clean_str(&f.version),
        clean_str(&a.id),
        if f.fixed_in.is_empty() {
            "no fix".to_owned()
        } else {
            format!("fixed in {}", clean_str(&f.fixed_in))
        },
        clean_str(&String::from_utf8_lossy(&f.path)),
        match wire::DepFindingState::try_from(f.state) {
            Ok(wire::DepFindingState::Dismissed) => "\tdismissed",
            Ok(wire::DepFindingState::Fixed) => "\tfixed",
            _ => "",
        }
    )
}

pub async fn run(ctx: &Ctx, a: ScanArgs) -> Result<ExitCode> {
    let remote = remote_with_token(a.remote.as_deref(), a.token_env.as_deref(), |k| {
        std::env::var(k).ok()
    })?;
    let ecos = ecosystems(&a.ecosystems)?;
    if let Some(r) = &a.rev
        && (r.is_empty() || r.len() > 1024)
    {
        return Err(bad("--rev is a revision"));
    }
    let conn = Conn::open(ctx, remote.as_deref()).await?;
    let repo = conn.repo_ref();
    let r = conn
        .call(false, |s, _| {
            let m = wire::ScanDepsRequest {
                repo: Some(repo.clone()),
                rev: a.rev.clone().unwrap_or_default(),
                ecosystems: ecos.clone(),
                refresh: a.refresh,
            };
            async move {
                Ok(client(&s)
                    .scan(s.wire.request(m, None))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.ScanDepsResponse", &r)?;
        } else {
            for f in &r.findings {
                writeln!(out, "{}", finding_line(f))?;
            }
            writeln!(
                out,
                "# {} files, {} packages, {} findings; advisory lookup {} ({}); {} packages unknown{}",
                r.files.len(),
                r.packages,
                r.findings.len(),
                lookup_name(r.lookup),
                clean_str(&r.source),
                r.unknown_packages,
                if r.truncated { "; the scan was cut short" } else { "" }
            )?;
            for e in &r.lookup_errors {
                writeln!(out, "# {}", clean_str(e))?;
            }
            for f in r.files.iter().filter(|f| !f.error.is_empty()) {
                writeln!(
                    out,
                    "# {}: {}",
                    clean_str(&String::from_utf8_lossy(&f.path)),
                    clean_str(&f.error)
                )?;
            }
        }
        // An unknown answer is not a clean one: it does not exit 0.
        Ok(if !r.findings.is_empty() {
            ExitCode::from(1)
        } else if r.lookup != wire::DepLookupStatus::Complete as i32 || r.truncated {
            ExitCode::from(2)
        } else {
            ExitCode::SUCCESS
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finding_is_one_line_with_its_fix_and_file() {
        let f = wire::DepFinding {
            ecosystem: "npm".into(),
            name: "lodash".into(),
            version: "4.17.20".into(),
            path: bytes::Bytes::from_static(b"web/package-lock.json"),
            fixed_in: "4.17.21".into(),
            advisory: Some(wire::DepAdvisory {
                id: "GHSA-x".into(),
                severity: wire::DepSeverity::High as i32,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            finding_line(&f),
            "high\tnpm lodash 4.17.20\tGHSA-x\tfixed in 4.17.21\tweb/package-lock.json"
        );
        let g = wire::DepFinding {
            fixed_in: String::new(),
            state: wire::DepFindingState::Dismissed as i32,
            name: "a\u{1b}[31mb".into(),
            ..f
        };
        let l = finding_line(&g);
        assert!(l.contains("no fix") && l.ends_with("\tdismissed"), "{l}");
        assert!(!l.contains('\u{1b}'), "terminal escapes are neutralised");
    }
}
