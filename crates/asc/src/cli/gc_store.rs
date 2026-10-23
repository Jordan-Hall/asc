//! `asc gc --store <url>`: garbage collection of a bucket-mode repository (SPEC §3.6, unit O01).
//!
//! The local `asc gc` (U40) calls [`run`] when `--store` is given. The marks live in the bucket
//! under `gc/<run>/`, one collection runs at a time (`gc/lock`, a 6 h lease), and a sweep stands
//! `gc/freeze` so a bucket-mode push aborts before its ref CAS. It never prompts: `--dry-run`
//! prints what would be deleted and changes no object.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;
use std::time::SystemTime;

use asc_store::gc::{BucketGc, GcConfig, GcError};

use crate::cli::common::CliError;

/// Longest accepted `--pitr-days` (ten years).
const MAX_PITR_DAYS: u64 = 3650;
/// The SPEC §3.6 default for `asc.gc.pitrDays`.
const DEFAULT_PITR_DAYS: u64 = 14;

/// Parsed `asc gc --store` arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreGcArgs {
    pub url: String,
    pub dry_run: bool,
    /// `asc.gc.pitrDays`; must be at least the Postgres PITR window of any server sharing the bucket.
    pub pitr_days: u64,
}

fn usage(msg: &str) -> CliError {
    CliError::Usage(format!(
        "error: {msg}\nusage: asc gc --store <url> [--dry-run] [--pitr-days <n>]"
    ))
}

impl StoreGcArgs {
    /// `default_pitr_days` is the value of `asc.gc.pitrDays` (14 when unset).
    pub fn parse(args: &[OsString], default_pitr_days: Option<u64>) -> Result<Self, CliError> {
        let mut url = None;
        let mut dry_run = false;
        let mut pitr = default_pitr_days.unwrap_or(DEFAULT_PITR_DAYS);
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let a = a.to_str().ok_or_else(|| usage("arguments must be UTF-8"))?;
            let (flag, inline) = match a.split_once('=') {
                Some((f, v)) if f.starts_with("--") => (f, Some(v.to_owned())),
                _ => (a, None),
            };
            let mut value = |name: &str| -> Result<String, CliError> {
                match inline.clone() {
                    Some(v) => Ok(v),
                    None => it
                        .next()
                        .and_then(|v| v.to_str().map(str::to_owned))
                        .ok_or_else(|| usage(&format!("{name} needs a value"))),
                }
            };
            match flag {
                "--store" => url = Some(value("--store")?),
                "--pitr-days" => {
                    pitr = value("--pitr-days")?
                        .parse()
                        .map_err(|_| usage("--pitr-days must be a whole number"))?;
                }
                "--dry-run" | "-n" if inline.is_none() => dry_run = true,
                other => return Err(usage(&format!("unknown argument {other:?}"))),
            }
        }
        if !(1..=MAX_PITR_DAYS).contains(&pitr) {
            return Err(usage("--pitr-days must be between 1 and 3650"));
        }
        let url = url.ok_or_else(|| usage("--store <url> is required"))?;
        Ok(Self {
            url,
            dry_run,
            pitr_days: pitr,
        })
    }

    fn config(&self) -> GcConfig {
        GcConfig {
            pitr: std::time::Duration::from_secs(self.pitr_days * 24 * 3600),
            ..GcConfig::default()
        }
    }
}

fn fatal(e: GcError) -> CliError {
    match e {
        GcError::Busy(m) => {
            CliError::Fatal(format!("asc: garbage collection is already running ({m})"))
        }
        other => CliError::Fatal(format!("asc: gc: {other}")),
    }
}

/// Runs one collection at `now` and prints its report to `out`.
pub async fn run_at(
    args: &StoreGcArgs,
    now: SystemTime,
    out: &mut dyn Write,
) -> Result<ExitCode, CliError> {
    let gc = BucketGc::open(&args.url, None).await.map_err(fatal)?;
    let report = gc
        .run(now, args.dry_run, &args.config(), &[])
        .await
        .map_err(fatal)?;
    write!(out, "{report}")?;
    Ok(ExitCode::SUCCESS)
}

/// `asc gc --store <url>`.
pub async fn run(args: &StoreGcArgs) -> Result<ExitCode, CliError> {
    run_at(args, SystemTime::now(), &mut std::io::stdout().lock()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_flags_and_the_configured_default() {
        let a = StoreGcArgs::parse(&os(&["--store", "s3://b/p", "--dry-run"]), None).unwrap();
        assert_eq!(
            a,
            StoreGcArgs {
                url: "s3://b/p".into(),
                dry_run: true,
                pitr_days: 14
            }
        );
        let b =
            StoreGcArgs::parse(&os(&["--store=file:///x", "--pitr-days=30"]), Some(21)).unwrap();
        assert_eq!(
            (b.url.as_str(), b.dry_run, b.pitr_days),
            ("file:///x", false, 30)
        );
        let c = StoreGcArgs::parse(&os(&["--store", "memory:///"]), Some(21)).unwrap();
        assert_eq!(c.pitr_days, 21);
    }

    #[test]
    fn refuses_bad_arguments() {
        for bad in [
            vec![],
            vec!["--store"],
            vec!["--store", "s3://b", "--pitr-days", "0"],
            vec!["--store", "s3://b", "--pitr-days", "x"],
            vec!["--store", "s3://b", "--pitr-days", "99999"],
            vec!["--store", "s3://b", "--force"],
            vec!["--store", "s3://b", "--dry-run=yes"],
        ] {
            assert!(
                matches!(StoreGcArgs::parse(&os(&bad), None), Err(CliError::Usage(_))),
                "{bad:?}"
            );
        }
    }

    fn file_url(dir: &std::path::Path) -> String {
        format!(
            "file:///{}",
            dir.display()
                .to_string()
                .replace('\\', "/")
                .trim_start_matches('/')
        )
    }

    #[tokio::test]
    async fn dry_run_on_an_empty_bucket_prints_a_report_and_leaves_no_lock() {
        let dir = tempfile::tempdir().unwrap();
        let args = StoreGcArgs {
            url: file_url(dir.path()),
            dry_run: true,
            pitr_days: 14,
        };
        let mut out = Vec::new();
        run_at(&args, SystemTime::now(), &mut out).await.unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("would delete 0 objects"), "{text}");
        assert!(
            text.contains("nothing is deleted until the next run"),
            "{text}"
        );
        assert!(!dir.path().join("gc").join("lock").exists());
        assert!(!dir.path().join("gc").join("freeze").exists());
        // A second run while a lease stands is refused.
        std::fs::write(
            dir.path().join("gc").join("lock"),
            format!("1 {}\n", i64::MAX / 2),
        )
        .unwrap();
        let err = run_at(&args, SystemTime::now(), &mut Vec::new())
            .await
            .unwrap_err();
        assert!(
            matches!(err, CliError::Fatal(ref m) if m.contains("already running")),
            "{err}"
        );
    }
}
