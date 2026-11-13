//! `asc deps scan`.

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
        "{}\t{} {} {}\t{}\t{}\t{}{}{}",
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
        a.intel.as_ref().map(intel_text).unwrap_or_default(),
        match wire::DepFindingState::try_from(f.state) {
            Ok(wire::DepFindingState::Dismissed) => "\tdismissed",
            Ok(wire::DepFindingState::Fixed) => "\tfixed",
            _ => "",
        }
    )
}

/// What the exploitation feeds say, as tab-led columns: `KEV` with its due date, `EPSS 0.87 (p99)`, or
/// `intel unknown`. Nothing for an advisory no feed lists.
fn intel_text(i: &wire::DepIntel) -> String {
    match wire::DepIntelStatus::try_from(i.status) {
        Ok(wire::DepIntelStatus::Unknown) => "\tintel unknown".to_owned(),
        Ok(wire::DepIntelStatus::Listed) => {
            let mut s = String::new();
            if i.kev {
                s.push_str("\tKEV");
                if !i.kev_due.is_empty() {
                    s.push_str(&format!(" due {}", clean_str(&i.kev_due)));
                }
                if i.kev_ransomware {
                    s.push_str(" ransomware");
                }
            }
            let unit = |x: f64| x.is_finite() && (0.0..=1.0).contains(&x);
            if (i.epss > 0.0 || i.epss_percentile > 0.0) && unit(i.epss) && unit(i.epss_percentile)
            {
                s.push_str(&format!(
                    "\tEPSS {:.2} (p{})",
                    i.epss,
                    (i.epss_percentile * 100.0).round() as u32
                ));
            }
            s
        }
        _ => String::new(),
    }
}

/// One line for a feed that is not current: its state and age, nothing else. None for a current feed or
/// one an older server did not report.
pub(crate) fn feed_line(name: &str, feed: Option<&wire::DepIntelFeeds>) -> Option<String> {
    let feed = feed?;
    let state = match wire::DepFeedState::try_from(feed.state) {
        Ok(wire::DepFeedState::Unconfigured) => "unconfigured",
        Ok(wire::DepFeedState::Unknown) => "unknown",
        _ => return None,
    };
    let age = if feed.loaded_at > 0 {
        format!(", age {}", age_text(feed.age_seconds))
    } else {
        String::new()
    };
    Some(format!("# {name} feed: {state}{age}"))
}

fn age_text(secs: i64) -> String {
    match secs.max(0) {
        s if s >= 86_400 => format!("{}d", s / 86_400),
        s if s >= 3_600 => format!("{}h", s / 3_600),
        s => format!("{}m", s / 60),
    }
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
            async move { Ok(client(&s).scan(s.wire.request(m, None)).await?.into_inner()) }
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
                if r.truncated {
                    "; the scan was cut short"
                } else {
                    ""
                }
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

    fn with_intel(i: Option<wire::DepIntel>) -> wire::DepFinding {
        wire::DepFinding {
            ecosystem: "npm".into(),
            name: "x".into(),
            version: "1".into(),
            advisory: Some(wire::DepAdvisory {
                id: "CVE-1".into(),
                intel: i,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn intel_shows_kev_epss_or_unknown_and_nothing_when_not_listed() {
        use wire::DepIntelStatus as S;
        let intel = |status: S| wire::DepIntel {
            status: status as i32,
            ..Default::default()
        };
        let both = wire::DepIntel {
            kev: true,
            kev_due: "2025-02-01".into(),
            kev_ransomware: true,
            epss: 0.87,
            epss_percentile: 0.99,
            ..intel(S::Listed)
        };
        let l = finding_line(&with_intel(Some(both)));
        assert!(
            l.ends_with("\tKEV due 2025-02-01 ransomware\tEPSS 0.87 (p99)"),
            "{l}"
        );
        let only_epss = wire::DepIntel {
            epss: 0.031,
            epss_percentile: 0.5,
            ..intel(S::Listed)
        };
        let l = finding_line(&with_intel(Some(only_epss)));
        assert!(
            l.ends_with("\tEPSS 0.03 (p50)") && !l.contains("KEV"),
            "{l}"
        );
        let kev_only = wire::DepIntel {
            kev: true,
            ..intel(S::Listed)
        };
        assert!(finding_line(&with_intel(Some(kev_only))).ends_with("\tKEV"));
        let l = finding_line(&with_intel(Some(intel(S::Unknown))));
        assert!(l.ends_with("\tintel unknown"), "{l}");
        let plain = finding_line(&with_intel(None));
        assert_eq!(finding_line(&with_intel(Some(intel(S::NotListed)))), plain);
        assert_eq!(
            finding_line(&with_intel(Some(intel(S::Unspecified)))),
            plain
        );
    }

    #[test]
    fn an_epss_value_out_of_range_is_not_shown_but_kev_is() {
        for (e, p) in [
            (f64::NAN, 0.5),
            (0.5, f64::INFINITY),
            (-0.1, 0.5),
            (0.5, 1.5),
            (2.0, 0.5),
        ] {
            let l = finding_line(&with_intel(Some(wire::DepIntel {
                status: wire::DepIntelStatus::Listed as i32,
                kev: true,
                epss: e,
                epss_percentile: p,
                ..Default::default()
            })));
            assert!(l.ends_with("	KEV") && !l.contains("EPSS"), "{e} {p}: {l}");
        }
    }

    #[test]
    fn hostile_server_text_is_cleaned() {
        let mut f = with_intel(Some(wire::DepIntel {
            status: wire::DepIntelStatus::Listed as i32,
            kev: true,
            kev_due: "\u{1b}[2Jdue".into(),
            ..Default::default()
        }));
        f.advisory.as_mut().unwrap().id = "CVE\u{1b}]0;pwn\u{7}".into();
        let l = finding_line(&f);
        assert!(!l.contains('\u{1b}') && !l.contains('\u{7}'), "{l}");
    }

    #[test]
    fn only_a_feed_that_is_not_current_gets_a_line() {
        use wire::DepFeedState as S;
        let feed = |state: S, loaded_at: i64, age: i64| wire::DepIntelFeeds {
            state: state as i32,
            loaded_at,
            age_seconds: age,
            ..Default::default()
        };
        assert_eq!(feed_line("kev", None), None);
        assert_eq!(feed_line("kev", Some(&feed(S::Current, 5, 60))), None);
        assert_eq!(
            feed_line("kev", Some(&feed(S::Unconfigured, 0, 0))).as_deref(),
            Some("# kev feed: unconfigured")
        );
        assert_eq!(
            feed_line("epss", Some(&feed(S::Unknown, 5, 172_800))).as_deref(),
            Some("# epss feed: unknown, age 2d")
        );
        assert_eq!(age_text(7_200), "2h");
        assert_eq!(age_text(-5), "0m");
    }
}
