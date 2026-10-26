//! `asc live pause|resume|status [--json]` (SPEC §21.3.8; B31).
//!
//! Asc-only verbs for the daemon's live updates. The pause is per worktree and is kept in a small
//! file under `$ASC_HOME/live/`, so it survives a daemon restart; the daemon's live driver (B30)
//! reads it through [`paused`] before every run. All three verbs need the daemon: with none
//! running (or `ASC_NO_DAEMON` set) they fail with [`ABSENT`] and change nothing.
//!
//! A live conflict pauses the worktree with reason `conflict`; a live run records that with
//! [`record_conflict`]. [`notice`] is what `asc status` and `asc sl` print until `asc live resume`.
//! A pause the user asked for (`user`) is shown by `asc live status` only.
//!
//! The daemon's production `Runner` (`daemon::live::prod`) starts the hidden verb `asc live run`
//! in each worktree it considers. That verb is one live pass (`daemon::live::pass`): fetch and,
//! per `asc.live.*`, restack and move. It is a separate process so that no hook of the user's
//! runs (the daemon points `core.hooksPath` at an empty directory, [`hookless_dir`], and the verb
//! refuses to run without that) and so that it can take the worktree's locks like any command. It
//! records its result with [`record_run`]; the pass ends up in the op log with kind `live`.
//!
//! The state file is private to the user (directory `0700` on Unix), written by rename, read with
//! a size bound and never through a link. The verbs `pause`, `resume` and `status` read and
//! write that file only; `run` is the only one that reads the network.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use asc_state::AscHome;
use serde::{Deserialize, Serialize};

use super::common::{CliError, Ctx, Result};
use crate::daemon::lifecycle::{self, Probe};
use crate::native::NativeSpec;

/// What the verbs say when no daemon answers.
pub const ABSENT: &str = "asc: the daemon is not running; start it with `asc daemon start`";

/// The most bytes read from a state file.
const MAX_STATE: u64 = 64 * 1024;
/// The most state files listed by `status`.
const MAX_LISTED: usize = 256;
/// The longest stored text (a path or a detail line).
const MAX_TEXT: usize = 1024;

#[derive(clap::Parser, Debug)]
#[command(name = "live", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[command(subcommand)]
    pub action: Action,
}

#[derive(clap::Subcommand, Debug)]
pub enum Action {
    /// Stop live updates for this worktree until `resume`.
    Pause,
    /// Resume live updates for this worktree and clear the conflict notice.
    Resume,
    /// Show the mode, the last run and the pause reason of each worktree.
    Status {
        /// Print one JSON document.
        #[arg(long)]
        json: bool,
    },
    /// One live pass for this worktree (started by the daemon; needs its empty hooks path).
    #[command(hide = true)]
    Run,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "live",
    flags: &["--json"],
    placeholders: &[],
}];

/// Why a worktree is paused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reason {
    /// `asc live pause`.
    User,
    /// A live restack or update hit a conflict.
    Conflict,
}

impl Reason {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Conflict => "conflict",
        }
    }
}

/// The result of the last live run, written by the daemon.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastRun {
    /// Unix seconds.
    pub at: i64,
    /// A short word such as `fetched`, `restacked`, `updated` or `notice`.
    pub outcome: String,
}

/// The stored state of one worktree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    pub worktree: String,
    pub repo: i64,
    pub paused: bool,
    #[serde(default)]
    pub reason: Option<Reason>,
    #[serde(default)]
    pub detail: String,
    /// Unix seconds of the pause.
    #[serde(default)]
    pub since: Option<i64>,
    #[serde(default)]
    pub last_run: Option<LastRun>,
}

impl State {
    fn fresh(worktree: &str, repo: i64) -> Self {
        Self {
            version: 1,
            worktree: clip(worktree),
            repo,
            paused: false,
            reason: None,
            detail: String::new(),
            since: None,
            last_run: None,
        }
    }
}

fn clip(text: &str) -> String {
    text.chars().take(MAX_TEXT).collect()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn dir(home: &AscHome) -> PathBuf {
    home.root().join("live")
}

/// A worktree id is a store-generated token; anything else never names a file.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn file(home: &AscHome, id: &str) -> Option<PathBuf> {
    valid_id(id).then(|| dir(home).join(format!("{id}.json")))
}

fn io_err(what: &str, e: impl std::fmt::Display) -> CliError {
    CliError::Raw(format!("asc: live: {what}: {e}"), 1)
}

/// Reads one state file: a regular file (not a link) of at most [`MAX_STATE`] bytes.
fn read_file(path: &Path) -> Option<State> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_STATE {
        return None;
    }
    let mut text = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_STATE)
        .read_to_end(&mut text)
        .ok()?;
    serde_json::from_slice(&text).ok()
}

/// The state of worktree `id`, if one was recorded.
pub fn load(home: &AscHome, id: &str) -> Option<State> {
    read_file(&file(home, id)?)
}

fn store(home: &AscHome, id: &str, state: &State) -> Result<()> {
    let path = file(home, id).ok_or_else(|| io_err("worktree id", "not a valid id"))?;
    let d = dir(home);
    std::fs::create_dir_all(&d).map_err(|e| io_err("create the state directory", e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700));
    }
    let body = serde_json::to_vec(state).map_err(|e| io_err("encode", e))?;
    // Write beside the target and rename over it: a reader sees the old or the new state.
    let tmp = d.join(format!(".{id}.{}.tmp", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .and_then(|mut f| std::io::Write::write_all(&mut f, &body))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(io_err("write", e));
    }
    Ok(())
}

/// Whether live mode is paused for worktree `id` (the daemon's `Runner::paused`).
pub fn paused(home: &AscHome, id: &str) -> bool {
    load(home, id).is_some_and(|s| s.paused)
}

/// The directory the daemon sets `core.hooksPath` to for a live pass, so that no hook runs on a
/// live update (SPEC §21.3.8). It is created empty and kept empty.
pub fn hookless_dir(home: &AscHome) -> PathBuf {
    dir(home).join("no-hooks")
}

/// Creates [`hookless_dir`] and returns it.
pub fn ensure_hookless_dir(home: &AscHome) -> std::io::Result<PathBuf> {
    let path = hookless_dir(home);
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

/// Records the result of a live run of worktree `id`: a short word such as `fetched`,
/// `restacked`, `updated` or `notice`. The pause (if any) is left as it is.
pub fn record_run(home: &AscHome, id: &str, worktree: &str, repo: i64, outcome: &str) -> Result<()> {
    let mut s = load(home, id).unwrap_or_else(|| State::fresh(worktree, repo));
    s.last_run = Some(LastRun {
        at: now(),
        outcome: clip(outcome),
    });
    store(home, id, &s)
}

/// Pauses worktree `id` because a live run hit a conflict (called by a live pass).
pub fn record_conflict(
    home: &AscHome,
    id: &str,
    worktree: &str,
    repo: i64,
    detail: &str,
) -> Result<()> {
    let mut s = load(home, id).unwrap_or_else(|| State::fresh(worktree, repo));
    s.paused = true;
    s.reason = Some(Reason::Conflict);
    s.detail = clip(detail);
    s.since = Some(now());
    store(home, id, &s)
}

/// The line `asc status` and `asc sl` print while a live conflict pauses this worktree.
pub fn notice(home: &AscHome, id: &str) -> Option<String> {
    let s = load(home, id)?;
    (s.paused && s.reason == Some(Reason::Conflict)).then(|| {
        let mut line = String::from(
            "asc: live updates are paused by a conflict; resolve it, then run `asc live resume`",
        );
        if !s.detail.is_empty() {
            line.push_str(" (");
            line.push_str(&sanitize(&s.detail));
            line.push(')');
        }
        line
    })
}

/// Control characters never reach the terminal.
fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn mode_of(ctx: &Ctx) -> &'static str {
    let on = |k: &str, d: bool| ctx.config.bool(k).unwrap_or(d);
    if on("asc.live.update", false) {
        "update"
    } else if on("asc.live.restack", false) {
        "restack"
    } else if on("asc.live.fetch", true) {
        "fetch"
    } else {
        "off"
    }
}

async fn require_daemon() -> Result<()> {
    if lifecycle::disabled(&|k: &str| std::env::var_os(k)) {
        return Err(CliError::Raw(ABSENT.into(), 1));
    }
    let (cfg, _) = super::daemon::system()?;
    match lifecycle::probe(&cfg).await {
        Probe::Up(_) => Ok(()),
        Probe::Hung | Probe::Down => Err(CliError::Raw(ABSENT.into(), 1)),
    }
}

fn describe(s: &State, mode: &str) -> String {
    let mut out = format!("{}: ", s.worktree);
    if s.paused {
        let reason = s.reason.map_or("user", Reason::as_str);
        out.push_str(&format!("paused ({reason})"));
        if !s.detail.is_empty() {
            out.push_str(&format!(": {}", sanitize(&s.detail)));
        }
    } else {
        out.push_str(&format!("live ({mode})"));
    }
    match &s.last_run {
        Some(r) => out.push_str(&format!("; last run {} ({})", r.at, sanitize(&r.outcome))),
        None => out.push_str("; last run never"),
    }
    out
}

fn json_of(s: &State, mode: &str, id: &str, current: bool) -> serde_json::Value {
    serde_json::json!({
        "worktree": s.worktree,
        "id": id,
        "current": current,
        "mode": if s.paused { "paused" } else { mode },
        "configured": mode,
        "paused": s.paused,
        "pauseReason": s.reason.filter(|_| s.paused).map(Reason::as_str),
        "detail": if s.paused { s.detail.as_str() } else { "" },
        "pausedSince": s.since.filter(|_| s.paused),
        "lastRun": s.last_run,
    })
}

/// `asc live run`: one pass, refused unless the daemon's empty hooks path is in force.
async fn pass(ctx: &Ctx) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let wanted = hookless_dir(&ws.home);
    let same = ctx.config.path("core.hooksPath").is_some_and(|p| {
        std::fs::canonicalize(&p)
            .ok()
            .zip(std::fs::canonicalize(&wanted).ok())
            .is_some_and(|(a, b)| a == b)
    });
    if !same {
        return Err(CliError::Raw(
            "asc: live run is started by the daemon; it must run without hooks".into(),
            1,
        ));
    }
    let id = ws.workspace.ws.id.to_string();
    let path = ws.workspace.ws.path.clone();
    let repo = ws.workspace.ws.repo.0;
    match crate::daemon::live::pass::run(ctx).await {
        Ok(done) => {
            record_run(&ws.home, &id, &path, repo, done.word())?;
            println!("{path}: {}", done.describe());
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            // The next window tries again; the failure shows in `asc live status`.
            let _ = record_run(&ws.home, &id, &path, repo, "failed");
            Err(e)
        }
    }
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    if matches!(args.action, Action::Run) {
        return pass(ctx).await;
    }
    require_daemon().await?;
    let ws = ctx.ws()?;
    let id = ws.workspace.ws.id.to_string();
    let repo = ws.workspace.ws.repo.0;
    let path = ws.workspace.ws.path.clone();
    match args.action {
        Action::Pause => {
            let mut s = load(&ws.home, &id).unwrap_or_else(|| State::fresh(&path, repo));
            if !s.paused {
                s.paused = true;
                s.reason = Some(Reason::User);
                s.detail.clear();
                s.since = Some(now());
                store(&ws.home, &id, &s)?;
            }
            println!("live updates paused for {path}");
        }
        Action::Resume => {
            if let Some(mut s) = load(&ws.home, &id) {
                s.paused = false;
                s.reason = None;
                s.detail.clear();
                s.since = None;
                store(&ws.home, &id, &s)?;
            }
            println!("live updates resumed for {path}");
        }
        // Answered before the daemon check; the arm keeps the match exhaustive.
        Action::Run => return pass(ctx).await,
        Action::Status { json } => {
            let mode = mode_of(ctx);
            let mut rows = vec![(
                id.clone(),
                load(&ws.home, &id).unwrap_or_else(|| State::fresh(&path, repo)),
                true,
            )];
            if let Ok(entries) = std::fs::read_dir(dir(&ws.home)) {
                for e in entries.flatten().take(MAX_LISTED) {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let Some(other) = name.strip_suffix(".json") else {
                        continue;
                    };
                    if other == id || !valid_id(other) {
                        continue;
                    }
                    if let Some(s) = read_file(&e.path()).filter(|s| s.repo == repo) {
                        rows.push((other.to_owned(), s, false));
                    }
                }
            }
            if json {
                let list: Vec<_> = rows
                    .iter()
                    .map(|(i, s, cur)| json_of(s, mode, i, *cur))
                    .collect();
                let doc = serde_json::json!({ "worktrees": list });
                println!(
                    "{}",
                    serde_json::to_string(&doc).map_err(|e| io_err("encode", e))?
                );
            } else {
                for (_, s, _) in &rows {
                    println!("{}", describe(s, mode));
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> (tempfile::TempDir, AscHome) {
        let t = tempfile::tempdir().unwrap();
        let h = AscHome::open(t.path().join("h")).unwrap();
        (t, h)
    }

    #[test]
    fn conflict_pause_shows_a_notice_until_cleared() {
        let (_t, h) = home();
        assert!(notice(&h, "w1").is_none());
        record_conflict(&h, "w1", "/wt", 1, "a.txt\nb").unwrap();
        assert!(paused(&h, "w1"));
        let n = notice(&h, "w1").unwrap();
        assert!(n.contains("asc live resume") && !n.contains('\n'));
    }

    #[test]
    fn a_user_pause_has_no_notice() {
        let (_t, h) = home();
        let mut s = State::fresh("/wt", 1);
        s.paused = true;
        s.reason = Some(Reason::User);
        store(&h, "w1", &s).unwrap();
        assert!(paused(&h, "w1") && notice(&h, "w1").is_none());
    }

    #[test]
    fn ids_cannot_escape_the_directory() {
        let (_t, h) = home();
        assert!(file(&h, "../x").is_none());
        assert!(file(&h, "a/b").is_none());
        assert!(file(&h, "").is_none());
        assert!(!paused(&h, "../x"));
    }

    #[test]
    fn oversized_or_garbled_state_reads_as_none() {
        let (_t, h) = home();
        let s = State::fresh("/wt", 1);
        store(&h, "w1", &s).unwrap();
        let p = file(&h, "w1").unwrap();
        std::fs::write(&p, vec![b' '; (MAX_STATE + 1) as usize]).unwrap();
        assert!(load(&h, "w1").is_none());
        std::fs::write(&p, b"{not json").unwrap();
        assert!(load(&h, "w1").is_none());
    }
}
