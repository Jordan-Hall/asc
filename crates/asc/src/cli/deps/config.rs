//! `asc deps config get | set`.

use std::process::ExitCode;

use asc_rpc::server::wire;

use super::{bad, client, ecosystems, kind_name, remote_with_token, severity_name};
use crate::cli::agent::conn::{Conn, clean_text};
use crate::cli::codeintel::{clean_str, emit_json, lines, with_stdout};
use crate::cli::common::{Ctx, Result};
use crate::cli::deps::findings::severity_of;
use crate::cli::deps::propose::max_kind;

#[derive(clap::Args, Debug)]
pub struct ConfigArgs {
    #[command(subcommand)]
    cmd: Sub,
}

#[derive(clap::Subcommand, Debug)]
enum Sub {
    /// Shows the settings (.asc/deps.toml) and the scheduled workflow that goes with them.
    Get(GetArgs),
    /// Proposes new settings as a change on an agent/deps ref; the settings in force are read from trunk.
    Set(SetArgs),
}

#[derive(clap::Args, Debug)]
struct GetArgs {
    #[arg(long)]
    rev: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long = "token-env")]
    token_env: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
struct SetArgs {
    /// Read only these ecosystems (OSV name or alias); repeat. Replaces the list.
    #[arg(long = "ecosystem")]
    ecosystems: Vec<String>,
    /// Read every ecosystem found (empties the list).
    #[arg(long = "all-ecosystems", conflicts_with = "ecosystems")]
    all_ecosystems: bool,
    /// Five-field cron expression, UTC, at most once an hour: run `asc deps propose` on a schedule.
    #[arg(long, conflicts_with = "no_schedule")]
    schedule: Option<String>,
    /// Remove the schedule (and the generated workflow).
    #[arg(long = "no-schedule")]
    no_schedule: bool,
    /// Propose only updates that fix an open advisory: true or false.
    #[arg(long = "security-only")]
    security_only: Option<bool>,
    /// Advisories below this are reported but not proposed for: low, moderate, high, critical or none.
    #[arg(long = "severity-threshold")]
    severity_threshold: Option<String>,
    /// The largest step a proposal makes: patch, minor or major.
    #[arg(long = "max-kind")]
    max_kind: Option<String>,
    /// Most changes one run records (1 to 50).
    #[arg(long = "max-proposals")]
    max_proposals: Option<u32>,
    /// How updates are grouped into changes: none, ecosystem, kind or all.
    #[arg(long)]
    grouping: Option<String>,
    /// A named group, `NAME=ECOSYSTEM:PATTERN,PATTERN[:KIND,KIND]`; repeat. Replaces the groups.
    #[arg(long = "group", conflicts_with = "no_groups")]
    groups: Vec<String>,
    /// Remove every group.
    #[arg(long = "no-groups")]
    no_groups: bool,
    /// An ignore rule, `ECOSYSTEM:NAME[:KIND,KIND][:until=YYYY-MM-DD][:reason=TEXT]` (`*` for any
    /// ecosystem); repeat. Replaces the rules.
    #[arg(long = "ignore", conflicts_with = "no_ignore")]
    ignore: Vec<String>,
    /// Remove every ignore rule.
    #[arg(long = "no-ignore")]
    no_ignore: bool,
    /// Show the files that would be written and record nothing.
    #[arg(long = "dry-run")]
    dry_run: bool,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long = "token-env")]
    token_env: Option<String>,
    #[arg(long)]
    json: bool,
}

fn kinds(list: &str) -> Result<Vec<i32>> {
    list.split(',')
        .filter(|k| !k.is_empty())
        .map(max_kind)
        .collect()
}

/// `NAME=ECOSYSTEM:PATTERN,PATTERN[:KIND,KIND]`.
pub(crate) fn parse_group(raw: &str) -> Result<wire::DepGroup> {
    let (name, rest) = raw.split_once('=').ok_or_else(|| {
        bad(format!(
            "--group {raw:?} is NAME=ECOSYSTEM:PATTERN[,PATTERN][:KIND[,KIND]]"
        ))
    })?;
    let mut parts = rest.splitn(3, ':');
    let eco = parts.next().unwrap_or("");
    let patterns: Vec<String> = parts
        .next()
        .unwrap_or("")
        .split(',')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect();
    if name.is_empty() || patterns.is_empty() {
        return Err(bad(format!(
            "--group {raw:?} needs a name and at least one package pattern"
        )));
    }
    let eco = if eco == "*" { "" } else { eco };
    if !eco.is_empty() {
        ecosystems(&[eco.to_owned()])?;
    }
    Ok(wire::DepGroup {
        name: name.to_owned(),
        ecosystem: eco.to_owned(),
        patterns,
        kinds: kinds(parts.next().unwrap_or(""))?,
    })
}

/// `ECOSYSTEM:NAME[:KIND,KIND][:until=YYYY-MM-DD][:reason=TEXT]`; the reason is the rest of the text.
pub(crate) fn parse_ignore(raw: &str) -> Result<wire::DepIgnore> {
    let usage = || {
        bad(format!(
            "--ignore {raw:?} is ECOSYSTEM:NAME[:KIND,KIND][:until=DATE][:reason=TEXT]"
        ))
    };
    let (eco, rest) = raw.split_once(':').ok_or_else(usage)?;
    if eco.is_empty() {
        return Err(usage());
    }
    let eco = if eco == "*" { "" } else { eco };
    if !eco.is_empty() {
        ecosystems(&[eco.to_owned()])?;
    }
    let parts: Vec<&str> = rest.split(':').collect();
    // The name is a package name, which may itself hold a colon (a Maven coordinate), so it ends at the
    // first part that is a kind list or a `key=value`.
    let mut i = 0;
    while i < parts.len() {
        let p = parts[i];
        let option = p.starts_with("until=")
            || p.starts_with("reason=")
            || (i > 0 && p.split(',').all(|k| max_kind(k).is_ok()));
        if option {
            break;
        }
        i += 1;
    }
    let name = parts[..i].join(":");
    if name.is_empty() {
        return Err(usage());
    }
    let mut out = wire::DepIgnore {
        ecosystem: eco.to_owned(),
        name,
        ..wire::DepIgnore::default()
    };
    while i < parts.len() {
        let p = parts[i];
        if p.starts_with("reason=") {
            out.reason = parts[i..]
                .join(":")
                .trim_start_matches("reason=")
                .to_owned();
            break;
        }
        if let Some(date) = p.strip_prefix("until=") {
            out.until = date.to_owned();
        } else {
            out.kinds = kinds(p)?;
        }
        i += 1;
    }
    Ok(out)
}

/// The current settings with the flags applied.
fn apply_flags(mut c: wire::DepsConfig, a: &SetArgs) -> Result<wire::DepsConfig> {
    if a.all_ecosystems {
        c.ecosystems.clear();
    } else if !a.ecosystems.is_empty() {
        c.ecosystems = ecosystems(&a.ecosystems)?;
    }
    if a.no_schedule {
        c.schedule.clear();
    } else if let Some(s) = &a.schedule {
        c.schedule.clone_from(s);
    }
    if let Some(b) = a.security_only {
        c.security_only = b;
    }
    if let Some(t) = &a.severity_threshold {
        c.severity_threshold = if t.eq_ignore_ascii_case("none") {
            wire::DepSeverity::Unspecified as i32
        } else {
            severity_of(t)?
        };
    }
    if let Some(k) = &a.max_kind {
        c.max_kind = max_kind(k)?;
    }
    if let Some(n) = a.max_proposals {
        c.max_proposals = n;
    }
    if let Some(g) = &a.grouping {
        c.grouping.clone_from(g);
    }
    if a.no_groups {
        c.groups.clear();
    } else if !a.groups.is_empty() {
        c.groups = a
            .groups
            .iter()
            .map(|g| parse_group(g))
            .collect::<Result<_>>()?;
    }
    if a.no_ignore {
        c.ignore.clear();
    } else if !a.ignore.is_empty() {
        c.ignore = a
            .ignore
            .iter()
            .map(|g| parse_ignore(g))
            .collect::<Result<_>>()?;
    }
    Ok(c)
}

fn changed_anything(a: &SetArgs) -> bool {
    a.all_ecosystems
        || !a.ecosystems.is_empty()
        || a.schedule.is_some()
        || a.no_schedule
        || a.security_only.is_some()
        || a.severity_threshold.is_some()
        || a.max_kind.is_some()
        || a.max_proposals.is_some()
        || a.grouping.is_some()
        || !a.groups.is_empty()
        || a.no_groups
        || !a.ignore.is_empty()
        || a.no_ignore
}

pub async fn run(ctx: &Ctx, a: ConfigArgs) -> Result<ExitCode> {
    match a.cmd {
        Sub::Get(x) => get(ctx, x).await,
        Sub::Set(x) => set(ctx, x).await,
    }
}

async fn get(ctx: &Ctx, a: GetArgs) -> Result<ExitCode> {
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

async fn set(ctx: &Ctx, a: SetArgs) -> Result<ExitCode> {
    if !changed_anything(&a) {
        return Err(bad("config set needs at least one setting to change"));
    }
    let remote = remote_with_token(a.remote.as_deref(), a.token_env.as_deref(), |k| {
        std::env::var(k).ok()
    })?;
    let conn = Conn::open(ctx, remote.as_deref()).await?;
    let repo = conn.repo_ref();
    let current = conn
        .call(false, |s, _| {
            let m = wire::GetDepsConfigRequest {
                repo: Some(repo.clone()),
                rev: String::new(),
            };
            async move {
                Ok(client(&s)
                    .get_config(s.wire.request(m, None))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    // A settings file the server cannot read is replaced by what the flags say on top of the defaults.
    let config = apply_flags(current.config.unwrap_or_default(), &a)?;
    let r = conn
        .call(!a.dry_run, |s, op| {
            let m = wire::SetDepsConfigRequest {
                repo: Some(repo.clone()),
                config: Some(config.clone()),
                dry_run: a.dry_run,
            };
            async move {
                Ok(client(&s)
                    .set_config(s.wire.request(m, op))
                    .await?
                    .into_inner())
            }
        })
        .await?;
    with_stdout(|out| {
        if a.json {
            emit_json(&mut lines(out), "asc.v1.SetDepsConfigResponse", &r)?;
        } else {
            writeln!(
                out,
                "{}\t{}",
                if r.changeset.is_empty() {
                    "plan"
                } else {
                    "proposed"
                },
                clean_str(&r.r#ref)
            )?;
            writeln!(out, "# .asc/deps.toml:\n{}", clean_text(&r.text))?;
            if !r.workflow.is_empty() {
                writeln!(
                    out,
                    "# .asc/workflows/deps.toml:\n{}",
                    clean_text(&r.workflow)
                )?;
            }
            writeln!(
                out,
                "# max kind {}, threshold {}",
                kind_name(config.max_kind),
                severity_name(config.severity_threshold)
            )?;
        }
        Ok(ExitCode::SUCCESS)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> SetArgs {
        SetArgs {
            ecosystems: vec![],
            all_ecosystems: false,
            schedule: None,
            no_schedule: false,
            security_only: None,
            severity_threshold: None,
            max_kind: None,
            max_proposals: None,
            grouping: None,
            groups: vec![],
            no_groups: false,
            ignore: vec![],
            no_ignore: false,
            dry_run: false,
            remote: None,
            token_env: None,
            json: false,
        }
    }

    #[test]
    fn groups_are_name_ecosystem_patterns_and_kinds() {
        let g = parse_group("lint=npm:eslint*,prettier:patch,minor").unwrap();
        assert_eq!(g.name, "lint");
        assert_eq!(g.ecosystem, "npm");
        assert_eq!(g.patterns, ["eslint*", "prettier"]);
        assert_eq!(
            g.kinds,
            [
                wire::DepUpdateKind::Patch as i32,
                wire::DepUpdateKind::Minor as i32
            ]
        );
        let any = parse_group("all=*:*").unwrap();
        assert!(any.ecosystem.is_empty() && any.kinds.is_empty());
        for bad in [
            "lint",
            "lint=npm",
            "=npm:x",
            "lint=cobol:x",
            "lint=npm:x:huge",
        ] {
            assert!(parse_group(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn ignore_rules_take_kinds_a_day_and_a_reason() {
        let i = parse_ignore("npm:left-pad").unwrap();
        assert_eq!((i.ecosystem.as_str(), i.name.as_str()), ("npm", "left-pad"));
        assert!(i.kinds.is_empty() && i.until.is_empty() && i.reason.is_empty());
        let i =
            parse_ignore("*:lodash*:major:until=2027-01-01:reason=pinned: by the vendor").unwrap();
        assert_eq!(i.ecosystem, "");
        assert_eq!(i.name, "lodash*");
        assert_eq!(i.kinds, [wire::DepUpdateKind::Major as i32]);
        assert_eq!(i.until, "2027-01-01");
        assert_eq!(i.reason, "pinned: by the vendor");
        // A Maven coordinate holds a colon.
        let i = parse_ignore("maven:org.apache.commons:commons-lang3:minor,patch").unwrap();
        assert_eq!(i.name, "org.apache.commons:commons-lang3");
        assert_eq!(i.kinds.len(), 2);
        let i = parse_ignore("maven:org.apache.commons:commons-lang3").unwrap();
        assert_eq!(i.name, "org.apache.commons:commons-lang3");
        for bad in ["npm", "cobol:x", ":x", "npm:", "npm:x:major:huge"] {
            assert!(parse_ignore(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn flags_change_only_what_they_name() {
        let base = wire::DepsConfig {
            version: 1,
            ecosystems: vec!["npm".into()],
            schedule: "0 4 * * 1".into(),
            max_proposals: 5,
            grouping: "kind".into(),
            groups: vec![wire::DepGroup {
                name: "g".into(),
                patterns: vec!["x".into()],
                ..wire::DepGroup::default()
            }],
            ..wire::DepsConfig::default()
        };
        let none = apply_flags(base.clone(), &args()).unwrap();
        assert_eq!(none, base);
        let a = SetArgs {
            no_schedule: true,
            security_only: Some(true),
            severity_threshold: Some("high".into()),
            max_kind: Some("patch".into()),
            all_ecosystems: true,
            no_groups: true,
            ..args()
        };
        let got = apply_flags(base.clone(), &a).unwrap();
        assert!(got.ecosystems.is_empty() && got.schedule.is_empty() && got.groups.is_empty());
        assert!(got.security_only);
        assert_eq!(got.severity_threshold, wire::DepSeverity::High as i32);
        assert_eq!(got.max_kind, wire::DepUpdateKind::Patch as i32);
        assert_eq!((got.max_proposals, got.grouping.as_str()), (5, "kind"));
        let a = SetArgs {
            severity_threshold: Some("none".into()),
            ..args()
        };
        let mut had = base;
        had.severity_threshold = wire::DepSeverity::High as i32;
        assert_eq!(
            apply_flags(had, &a).unwrap().severity_threshold,
            wire::DepSeverity::Unspecified as i32
        );
        let a = SetArgs {
            severity_threshold: Some("severe".into()),
            ..args()
        };
        assert!(apply_flags(wire::DepsConfig::default(), &a).is_err());
        assert!(!changed_anything(&args()));
        assert!(changed_anything(&SetArgs {
            no_ignore: true,
            ..args()
        }));
    }
}
