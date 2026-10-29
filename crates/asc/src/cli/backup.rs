//! `asc backup list | targets | run | verify | schedule | restore`: scheduled backups of the server
//! (`Backups`; site administrators only). Restore is staged and applied when the server restarts.

use std::process::ExitCode;

use asc_rpc::server::wire;
use tonic::transport::Channel;

use super::acct::{bad, json, size};
use super::agent::conn::{Conn, MAX_MESSAGE, clean_text, when};
use super::codeintel::{clean_str, with_stdout};
use super::common::{Ctx, Result};
use crate::native::NativeSpec;
use asc_rpc::services::backups_client::BackupsClient;

fn client(s: &crate::remote::server::Session) -> BackupsClient<Channel> {
    BackupsClient::new(s.wire.channel.clone())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE)
}

#[derive(clap::Parser, Debug)]
#[command(
    name = "backup",
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
    /// The history, the schedule and the state of the backups.
    List(Common),
    /// The targets the server's configuration names.
    Targets(Common),
    /// Starts a backup now.
    Run(RunArgs),
    /// Checks a backup.
    Verify(IdArgs),
    /// Shows or changes the schedule.
    Schedule(ScheduleArgs),
    /// Stages a restore of a verified backup (applied at the next start).
    Restore(RestoreArgs),
}

#[derive(clap::Args, Debug)]
pub struct Common {
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct RunArgs {
    #[arg(long)]
    target: Option<String>,
    #[command(flatten)]
    common: Common,
}

#[derive(clap::Args, Debug)]
pub struct IdArgs {
    id: String,
    #[command(flatten)]
    common: Common,
}

#[derive(clap::Args, Debug)]
pub struct ScheduleArgs {
    #[arg(long, conflicts_with = "disable")]
    enable: bool,
    #[arg(long)]
    disable: bool,
    /// Hours between backups, 1 to 168.
    #[arg(long = "every-hours")]
    every_hours: Option<u32>,
    /// Time of day (UTC) the interval is anchored to, `HH:MM`.
    #[arg(long)]
    at: Option<String>,
    #[arg(long)]
    target: Option<String>,
    #[arg(long = "keep-days")]
    keep_days: Option<u32>,
    #[arg(long = "keep-weeks")]
    keep_weeks: Option<u32>,
    #[command(flatten)]
    common: Common,
}

#[derive(clap::Args, Debug)]
pub struct RestoreArgs {
    id: String,
    /// Type RESTORE to confirm.
    #[arg(long)]
    confirm: Option<String>,
    #[command(flatten)]
    common: Common,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "backup",
    flags: &[
        "--remote",
        "--json",
        "--target",
        "--enable",
        "--disable",
        "--every-hours",
        "--at",
        "--keep-days",
        "--keep-weeks",
        "--confirm",
    ],
    placeholders: &[],
}];

/// `HH:MM` as minutes of the day.
pub(crate) fn minute_of_day(s: &str) -> Result<u32> {
    let ok = s.split_once(':').and_then(|(h, m)| {
        let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
        (h < 24 && m < 60 && s.len() == 5).then_some(h * 60 + m)
    });
    ok.ok_or_else(|| bad("backup", "--at is HH:MM, for example 02:00"))
}

/// The schedule `a` asks for, over the current one.
pub(crate) fn schedule_of(
    cur: wire::BackupSchedule,
    a: &ScheduleArgs,
) -> Result<wire::BackupSchedule> {
    let mut s = cur;
    if a.enable {
        s.enabled = true;
    }
    if a.disable {
        s.enabled = false;
    }
    if let Some(h) = a.every_hours {
        if !(1..=168).contains(&h) {
            return Err(bad("backup", "--every-hours is 1 to 168"));
        }
        s.interval_hours = h;
    }
    if let Some(at) = &a.at {
        s.minute_of_day = minute_of_day(at)?;
    }
    if let Some(t) = &a.target {
        s.target.clone_from(t);
    }
    if let Some(d) = a.keep_days {
        s.keep_days = d;
    }
    if let Some(w) = a.keep_weeks {
        s.keep_weeks = w;
    }
    Ok(s)
}

fn run_line(b: &wire::Backup) -> String {
    let state = super::agent::conn::short(
        wire::BackupState::try_from(b.state).map_or("?", |s| s.as_str_name()),
        "BACKUP_STATE_",
    );
    let verify = super::agent::conn::short(
        wire::BackupVerify::try_from(b.verify).map_or("?", |s| s.as_str_name()),
        "BACKUP_VERIFY_",
    );
    format!(
        "{}  {}  {}  {:<9} {:<12} {}  {}",
        clean_str(&b.id),
        when(b.started),
        clean_str(&b.target),
        state,
        verify,
        size(b.size_bytes),
        clean_str(&b.note)
    )
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    match a.cmd {
        Sub::List(c) => {
            let conn = Conn::open(ctx, c.remote.as_deref()).await?;
            let r = conn
                .call(false, |s, _| async move {
                    Ok(client(&s)
                        .list_backups(s.wire.request(wire::ListBackupsRequest::default(), None))
                        .await?
                        .into_inner())
                })
                .await?;
            if c.json {
                return json("asc.v1.ListBackupsResponse", &r);
            }
            with_stdout(|out| {
                if let Some(st) = &r.status {
                    writeln!(
                        out,
                        "asc: last success {}{}{}",
                        when(st.last_success),
                        if st.overdue { " (overdue)" } else { "" },
                        if st.restore_staged {
                            "; a restore is staged: the server is read-only until it restarts"
                        } else {
                            ""
                        }
                    )?;
                }
                for b in &r.backups {
                    writeln!(out, "{}", run_line(b))?;
                }
                Ok(ExitCode::SUCCESS)
            })
        }
        Sub::Targets(c) => {
            let conn = Conn::open(ctx, c.remote.as_deref()).await?;
            let r = conn
                .call(false, |s, _| async move {
                    Ok(client(&s)
                        .list_targets(s.wire.request(wire::ListBackupTargetsRequest {}, None))
                        .await?
                        .into_inner())
                })
                .await?;
            if c.json {
                return json("asc.v1.ListBackupTargetsResponse", &r);
            }
            with_stdout(|out| {
                for t in &r.targets {
                    writeln!(
                        out,
                        "{}  {}  {}{}",
                        clean_str(&t.name),
                        clean_str(&t.kind),
                        clean_str(&t.location),
                        if t.encrypted { "  encrypted" } else { "" }
                    )?;
                }
                Ok(ExitCode::SUCCESS)
            })
        }
        Sub::Run(x) => {
            let conn = Conn::open(ctx, x.common.remote.as_deref()).await?;
            let msg = wire::RunBackupRequest {
                target: x.target.unwrap_or_default(),
            };
            let r = conn
                .call(true, |s, op| {
                    let m = msg.clone();
                    async move {
                        Ok(client(&s)
                            .run_backup(s.wire.request(m, op))
                            .await?
                            .into_inner())
                    }
                })
                .await?;
            let b = r.backup.unwrap_or_default();
            if x.common.json {
                return json("asc.v1.Backup", &b);
            }
            println!("{}", run_line(&b));
            Ok(ExitCode::SUCCESS)
        }
        Sub::Verify(x) => {
            let conn = Conn::open(ctx, x.common.remote.as_deref()).await?;
            let msg = wire::VerifyBackupRequest { id: x.id };
            let r = conn
                .call(true, |s, op| {
                    let m = msg.clone();
                    async move {
                        Ok(client(&s)
                            .verify_backup(s.wire.request(m, op))
                            .await?
                            .into_inner())
                    }
                })
                .await?;
            let b = r.backup.unwrap_or_default();
            println!("{}", run_line(&b));
            Ok(ExitCode::SUCCESS)
        }
        Sub::Schedule(x) => {
            let conn = Conn::open(ctx, x.common.remote.as_deref()).await?;
            let cur = conn
                .call(false, |s, _| async move {
                    Ok(client(&s)
                        .list_backups(s.wire.request(wire::ListBackupsRequest { limit: 1 }, None))
                        .await?
                        .into_inner()
                        .schedule
                        .unwrap_or_default())
                })
                .await?;
            let changing = x.enable
                || x.disable
                || x.every_hours.is_some()
                || x.at.is_some()
                || x.target.is_some()
                || x.keep_days.is_some()
                || x.keep_weeks.is_some();
            let s = if changing {
                let want = schedule_of(cur, &x)?;
                let msg = wire::SetBackupScheduleRequest {
                    schedule: Some(want),
                };
                conn.call(true, |s, op| {
                    let m = msg.clone();
                    async move {
                        Ok(client(&s)
                            .set_schedule(s.wire.request(m, op))
                            .await?
                            .into_inner()
                            .schedule
                            .unwrap_or_default())
                    }
                })
                .await?
            } else {
                cur
            };
            if x.common.json {
                return json("asc.v1.BackupSchedule", &s);
            }
            println!(
                "{} every {} h at {:02}:{:02} UTC to {}; keep {} days then {} weeks; next {}",
                if s.enabled { "on" } else { "off" },
                s.interval_hours,
                s.minute_of_day / 60,
                s.minute_of_day % 60,
                clean_str(&s.target),
                s.keep_days,
                s.keep_weeks,
                when(s.next_run)
            );
            Ok(ExitCode::SUCCESS)
        }
        Sub::Restore(x) => {
            let conn = Conn::open(ctx, x.common.remote.as_deref()).await?;
            let id = x.id.clone();
            let p = conn
                .call(false, |s, _| {
                    let m = wire::PreviewRestoreRequest { id: id.clone() };
                    async move {
                        Ok(client(&s)
                            .preview_restore(s.wire.request(m, None))
                            .await?
                            .into_inner())
                    }
                })
                .await?;
            for w in &p.warnings {
                eprintln!("asc: warning: {}", clean_text(w));
            }
            eprintln!(
                "asc: restoring replaces the current data with the state at {}; everything since is lost",
                when(p.loses_changes_since)
            );
            let Some(word) = x.confirm.filter(|c| *c == p.confirmation) else {
                return Err(bad(
                    "backup",
                    format!("add --confirm {} to stage the restore", p.confirmation),
                ));
            };
            let msg = wire::RestoreBackupRequest {
                id: x.id,
                confirm: word,
            };
            let r = conn
                .call(true, |s, op| {
                    let m = msg.clone();
                    async move {
                        Ok(client(&s)
                            .restore(s.wire.request(m, op))
                            .await?
                            .into_inner())
                    }
                })
                .await?;
            eprintln!(
                "asc: restore of {} staged; restart the server to apply it. {}",
                clean_str(&r.backup_id),
                clean_text(&r.command)
            );
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(extra: &[&str]) -> Args {
        let mut v = vec!["backup", "schedule"];
        v.extend_from_slice(extra);
        <Args as clap::Parser>::try_parse_from(v).unwrap()
    }

    fn sched(extra: &[&str]) -> Result<wire::BackupSchedule> {
        let Sub::Schedule(a) = args(extra).cmd else {
            unreachable!()
        };
        schedule_of(
            wire::BackupSchedule {
                interval_hours: 24,
                minute_of_day: 120,
                keep_days: 30,
                keep_weeks: 52,
                target: "r2".into(),
                ..Default::default()
            },
            &a,
        )
    }

    #[test]
    fn the_schedule_flags_change_only_what_they_name() {
        let s = sched(&["--enable", "--every-hours", "6", "--at", "03:30"]).unwrap();
        assert_eq!(
            (
                s.enabled,
                s.interval_hours,
                s.minute_of_day,
                s.keep_days,
                s.target.as_str()
            ),
            (true, 6, 210, 30, "r2")
        );
        for bad in [
            &["--every-hours", "0"][..],
            &["--every-hours", "169"],
            &["--at", "25:00"],
            &["--at", "2:00"],
            &["--at", "0200"],
        ] {
            assert!(sched(bad).is_err(), "{bad:?}");
        }
        assert!(
            <Args as clap::Parser>::try_parse_from(["backup", "schedule", "--enable", "--disable"])
                .is_err()
        );
    }
}
