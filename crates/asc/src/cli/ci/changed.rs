//! `asc ci changed --repo R --base B --head H [--rules <path>|--rules-file <file>]` (SPEC
//! §12.2): the plan job's glob switches, as `key=value` lines on stdout for `$GITHUB_OUTPUT`.
//! The decisions live in [`asc_ci::changed`]. This file is the [`Backend`] that makes the server
//! calls through `asc api <Rpc>` and `asc affected --format gha` children (the same binary,
//! the same credentials), so the plan job needs no checkout and no workspace. The token comes
//! from `ASC_TOKEN` (or `GITHUB_TOKEN`), never from argv.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

use asc_ci::changed::{self, Backend, BackendError, Input, Plan, PlanError};
use async_trait::async_trait;
use clap::Parser;
use serde_json::{Value, json};

use crate::cli::common::{CliError, Ctx, Result};

/// Every flag of `ci changed` and `ci affected`, for the native table.
pub const FLAGS: &[&str] = &[
    "--repo",
    "--base",
    "--head",
    "--rules",
    "--rules-file",
    "--server",
];

/// Most changed paths asked of one `Diff`; a longer answer is cut and treated as a failure.
const MAX_FILES: u32 = 100_000;
/// Largest child output read.
const MAX_OUT: usize = 64 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(
    name = "ci changed",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub(super) struct Args {
    /// `owner/repo`, or the repository URL.
    #[arg(long)]
    pub(super) repo: String,
    #[arg(long, default_value = "master")]
    pub(super) base: String,
    #[arg(long, default_value = "HEAD")]
    pub(super) head: String,
    /// A repository path read at the merge base.
    #[arg(long)]
    pub(super) rules: Option<String>,
    /// A local rules file, read instead of `--rules`.
    #[arg(long)]
    pub(super) rules_file: Option<PathBuf>,
    /// The asc server's public URL.
    #[arg(long, env = "ASC_SERVER_URL")]
    pub(super) server: Option<String>,
}

pub(super) fn parse<T: Parser>(name: &str, rest: &[OsString]) -> Result<T> {
    T::try_parse_from(std::iter::once(OsString::from(name)).chain(rest.iter().cloned()))
        .map_err(|e| CliError::Usage(e.to_string()))
}

/// The remote URL `--repo` and `--server` name.
pub(super) fn remote_url(repo: &str, server: Option<&str>) -> Result<String> {
    if repo.contains("://") {
        return Ok(repo.to_owned());
    }
    let server = server
        .map(str::to_owned)
        .or_else(|| std::env::var("ASC_SERVER").ok().filter(|v| !v.is_empty()))
        .ok_or_else(|| {
            CliError::Fatal(
                "asc: --repo owner/name needs --server (or ASC_SERVER_URL), or pass a URL".into(),
            )
        })?;
    Ok(format!("{}/{repo}", server.trim_end_matches('/')))
}

fn bad_rev(s: &str) -> bool {
    s.is_empty() || s.starts_with('-') || s.chars().any(char::is_control) || s.len() > 1024
}

/// The server calls over `asc` children.
pub(super) struct Cli {
    exe: PathBuf,
    cwd: PathBuf,
    url: String,
}

fn b64_val(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Standard base64 (proto3 JSON `bytes`); `None` on anything else.
pub(super) fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in s {
        acc = (acc << 6) | u32::from(b64_val(c)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xff).ok()?);
        }
    }
    Some(out)
}

pub(super) fn b64_encode(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(char::from(T[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn hex(b: &[u8]) -> String {
    use std::fmt::Write as _;
    b.iter().fold(String::new(), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

impl Cli {
    pub(super) fn new(ctx: &Ctx, url: String) -> Result<Self> {
        Ok(Self {
            exe: std::env::current_exe()?,
            cwd: ctx.cwd.clone(),
            url,
        })
    }

    fn child(&self, args: &[&str]) -> std::io::Result<std::process::Output> {
        let mut c = Command::new(&self.exe);
        c.current_dir(&self.cwd).args(args).stdin(Stdio::null());
        for v in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
        ] {
            c.env_remove(v);
        }
        if std::env::var_os("ASC_TOKEN").is_none()
            && let Some(t) = std::env::var_os("GITHUB_TOKEN")
        {
            c.env("ASC_TOKEN", t);
        }
        c.output()
    }

    /// `asc <args>`; the stdout, or the one-line failure (the token never appears in argv).
    fn run(&self, args: &[&str]) -> std::result::Result<String, BackendError> {
        let out = self
            .child(args)
            .map_err(|e| BackendError(format!("cannot run asc: {e}")))?;
        if out.stdout.len() > MAX_OUT {
            return Err(BackendError("the answer is too large".into()));
        }
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let line: String = err
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("failed")
                .chars()
                .filter(|c| !c.is_control())
                .take(300)
                .collect();
            return Err(BackendError(line));
        }
        String::from_utf8(out.stdout).map_err(|_| BackendError("the answer is not UTF-8".into()))
    }

    /// `asc api <rpc> --json <req> --remote <url>`: every printed JSON value.
    fn api(&self, rpc: &str, req: &Value) -> std::result::Result<Vec<Value>, BackendError> {
        let text = self.run(&[
            "api",
            rpc,
            "--json",
            &req.to_string(),
            "--remote",
            &self.url,
        ])?;
        serde_json::Deserializer::from_str(&text)
            .into_iter::<Value>()
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| BackendError(format!("{rpc}: bad answer: {e}")))
    }

    fn check(&self, revs: &[&str]) -> std::result::Result<(), BackendError> {
        if revs.iter().any(|r| bad_rev(r)) {
            return Err(BackendError("not a usable revision".into()));
        }
        Ok(())
    }
}

#[async_trait]
impl Backend for Cli {
    async fn merge_base(
        &self,
        base: &str,
        head: &str,
    ) -> std::result::Result<String, BackendError> {
        self.check(&[base, head])?;
        let r = self.api("Resolve", &json!({ "revspecs": [base, head] }))?;
        let ids: Vec<Vec<u8>> = r
            .first()
            .and_then(|v| v["ids"].as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().and_then(b64_decode))
                    .collect()
            })
            .unwrap_or_default();
        let [a, b] = ids.as_slice() else {
            return Err(BackendError("a revision does not resolve".into()));
        };
        let r = self.api(
            "MergeBase",
            &json!({ "a": b64_encode(a), "b": [b64_encode(b)] }),
        )?;
        r.first()
            .and_then(|v| v["bases"].as_array())
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .and_then(b64_decode)
            .map(|b| hex(&b))
            .ok_or_else(|| BackendError("no merge base".into()))
    }

    async fn read_file(
        &self,
        rev: &str,
        path: &str,
    ) -> std::result::Result<Option<Vec<u8>>, BackendError> {
        self.check(&[rev, path])?;
        let req = json!({ "revPath": format!("{rev}:{path}") });
        let r = match self.api("ReadFile", &req) {
            Ok(r) => r,
            Err(e) if e.0.contains("does not exist") || e.0.contains("NotFound") => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let mut data = Vec::new();
        for v in &r {
            if let Some(chunk) = v["data"].as_str() {
                data.extend(b64_decode(chunk).ok_or_else(|| BackendError("bad data".into()))?);
            }
            if data.len() > asc_ci::rules::MAX_RULES_BYTES {
                return Err(BackendError("the rules file is too large".into()));
            }
        }
        Ok(Some(data))
    }

    async fn changed_paths(
        &self,
        base: &str,
        head: &str,
    ) -> std::result::Result<Vec<String>, BackendError> {
        self.check(&[base, head])?;
        let r = self.api(
            "Diff",
            &json!({
                "base": base,
                "head": head,
                "mergeBase": true,
                "format": "DIFF_FORMAT_NAMES",
                "nullTerminated": true,
                "maxFiles": MAX_FILES,
            }),
        )?;
        let mut paths = Vec::new();
        for v in &r {
            if v["truncated"].as_bool() == Some(true) {
                return Err(BackendError("the changed-path list is cut".into()));
            }
            for c in v["changes"].as_array().into_iter().flatten() {
                let raw = c["path"]
                    .as_str()
                    .and_then(b64_decode)
                    .ok_or_else(|| BackendError("bad path in the diff".into()))?;
                paths.push(String::from_utf8_lossy(&raw).into_owned());
            }
        }
        Ok(paths)
    }

    async fn semantic(
        &self,
        base: &str,
        head: &str,
    ) -> std::result::Result<Option<String>, BackendError> {
        self.check(&[base, head])?;
        let req = json!({ "base": base, "head": head, "mergeBase": true });
        match self.api("Classify", &req) {
            Ok(r) => Ok(r.first().and_then(|v| v["overall"].as_str()).map(|s| {
                s.strip_prefix("CHANGE_CLASS_")
                    .unwrap_or(s)
                    .to_ascii_lowercase()
            })),
            Err(e) if e.0.to_ascii_lowercase().contains("unimplemented") => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn affected_gha(
        &self,
        base: &str,
        head: &str,
    ) -> std::result::Result<String, BackendError> {
        self.check(&[base, head])?;
        self.run(&[
            "affected", "--repo", &self.url, "--base", base, "--head", head, "--format", "gha",
        ])
    }

    async fn affected_matches(
        &self,
        base: &str,
        head: &str,
        select: &[String],
    ) -> std::result::Result<bool, BackendError> {
        self.check(&[base, head])?;
        let req = json!({ "base": base, "head": head, "select": select });
        let r = self.api("Affected", &req)?;
        Ok(r.first()
            .and_then(|v| v["projects"].as_array())
            .is_some_and(|p| !p.is_empty()))
    }
}

/// The plan input from the command line.
pub(super) fn input(ctx: &Ctx, a: &Args) -> Result<Input> {
    let rules_file = a
        .rules_file
        .as_ref()
        .map(|p| std::path::absolute(ctx.cwd.join(p)))
        .transpose()?;
    Ok(Input {
        base: a.base.clone(),
        head: a.head.clone(),
        rules_path: a.rules.clone(),
        rules_file,
    })
}

/// Prints the warnings to stderr and the lines to stdout.
pub(super) fn emit(plan: &Plan) -> Result<ExitCode> {
    use std::io::Write as _;
    for w in &plan.warnings {
        eprintln!("warning: {w}");
    }
    let mut out = std::io::stdout().lock();
    out.write_all(plan.render().as_bytes())?;
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

pub(super) fn plan_err(e: PlanError) -> CliError {
    CliError::Fatal(format!("asc: ci: {e}"))
}

pub async fn run(ctx: &Ctx, rest: &[OsString]) -> Result<ExitCode> {
    let a: Args = parse("ci changed", rest)?;
    let backend = Cli::new(ctx, remote_url(&a.repo, a.server.as_deref())?)?;
    let plan = changed::changed(&backend, &input(ctx, &a)?)
        .await
        .map_err(plan_err)?;
    emit(&plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        for n in 0..40usize {
            let b: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(b64_decode(&b64_encode(&b)).as_deref(), Some(b.as_slice()));
        }
        assert_eq!(b64_encode(b"Man"), "TWFu");
        assert_eq!(b64_encode(b"Ma"), "TWE=");
        assert_eq!(b64_decode("TWE=").unwrap(), b"Ma");
        assert!(b64_decode("T*E=").is_none());
    }

    #[test]
    fn remote_url_forms() {
        assert_eq!(
            remote_url("a/b", Some("https://s.example/")).unwrap(),
            "https://s.example/a/b"
        );
        assert_eq!(
            remote_url("https://s.example/a/b", None).unwrap(),
            "https://s.example/a/b"
        );
    }

    #[test]
    fn revisions_cannot_be_options() {
        assert!(bad_rev("--upload-pack=x"));
        assert!(bad_rev("a\nb"));
        assert!(!bad_rev("refs/heads/master"));
    }
}
