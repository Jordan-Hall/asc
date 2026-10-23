//! Git-remote mode (SPEC §6.3): the real-git half. A private bare repository,
//! `repos/<id>/git-remote.git`, that real git fetches into and pushes from. Every git process
//! comes from [`RealGit::command`], never from `git` by name, so the shim cannot recurse.
//!
//! This module moves git objects and refs only. Importing what a fetch brought and exporting
//! what a push needs are the callers' jobs (`asc::remote::git`).
//!
//! The remote's refs live under `refs/ascr/<ns>/heads/*` and `refs/ascr/<ns>/tags/*` in the bare
//! repository, where `<ns>` names the remote. Nothing here ever prompts: `GIT_TERMINAL_PROMPT=0`
//! always, because asc is never interactive.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use asc_core::id::GitOid;

use crate::realgit::{RealGit, RealGitError};

/// Most refs read back from one `for-each-ref` or `ls-remote`.
pub const MAX_REFS: usize = 1_000_000;
/// Most bytes of git's stderr kept for an error message.
const MAX_STDERR: usize = 16 * 1024;
/// Most bytes of ref listing read from git.
const MAX_LISTING: usize = 512 * 1024 * 1024;

/// Why a git-remote operation failed.
#[derive(Debug, thiserror::Error)]
pub enum GitRemoteError {
    #[error(transparent)]
    RealGit(#[from] RealGitError),
    #[error("cannot run git: {0}")]
    Io(#[from] std::io::Error),
    /// Git ran and failed; `stderr` is its (bounded) message.
    #[error("{stderr}")]
    Git { stderr: String },
    #[error("{0}")]
    Url(String),
    #[error("git printed a ref line that does not parse: {0:?}")]
    BadRef(String),
    #[error("the remote has more than {MAX_REFS} refs")]
    TooManyRefs,
}

pub type Result<T, E = GitRemoteError> = std::result::Result<T, E>;

/// One push instruction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushRef {
    /// The full remote ref name, such as `refs/heads/master`.
    pub dst: String,
    /// The object to push; `None` deletes the remote ref.
    pub new: Option<GitOid>,
    pub lease: Lease,
}

/// What the remote ref must be for the push to proceed (`--force-with-lease`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lease {
    /// No check, and a non-fast-forward is allowed.
    Force,
    /// The ref must not exist.
    Absent,
    /// The ref must be exactly this object.
    Value(GitOid),
}

/// Why a push did not happen.
#[derive(Debug)]
pub enum PushFailure {
    /// A lease or fast-forward check failed on the remote ref.
    Stale { dst: String, reason: String },
    /// Anything else: transport, authentication, a hook on the server.
    Failed(String),
}

/// The refs a remote advertises.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Advertised {
    pub refs: Vec<(String, GitOid)>,
    /// What `HEAD` points to (`refs/heads/master`), when the remote said.
    pub head: Option<String>,
}

/// The URL real git is given for a stored remote URL: `git+https://` and `git+ssh://` lose the
/// `git+` prefix. A URL that git would treat as a remote helper (`ext::`, `fd::`, any
/// `<helper>::`) or as an option is refused: a remote URL is untrusted text.
pub fn transport_url(stored: &str) -> Result<String> {
    if stored.is_empty() || stored.starts_with('-') || stored.chars().any(char::is_control) {
        return Err(GitRemoteError::Url(format!(
            "fatal: asc: not a usable remote URL: {stored:?}"
        )));
    }
    let lower = stored.to_ascii_lowercase();
    let url = if lower.starts_with("git+") {
        &stored[4..]
    } else {
        stored
    };
    if let Some((helper, _)) = url.split_once("::")
        && !helper.is_empty()
        && helper
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-'))
    {
        return Err(GitRemoteError::Url(
            "fatal: asc: remote helper URLs (<helper>::<address>) are not supported".into(),
        ));
    }
    Ok(url.to_owned())
}

/// A namespace component for `refs/ascr/<ns>/` from a remote name: the name itself when it is
/// plain, otherwise its hex.
pub fn namespace(remote: &str) -> String {
    let plain = !remote.is_empty()
        && remote.len() <= 64
        && remote
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if plain {
        remote.to_owned()
    } else {
        let mut s = String::from("x");
        for b in remote.bytes().take(120) {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

fn clean_env(cmd: &mut Command) {
    for k in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
        "GIT_QUARANTINE_PATH",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
    ] {
        cmd.env_remove(k);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never");
}

fn safe_args(cmd: &mut Command, hooks: &Path) {
    cmd.arg("-c")
        .arg("protocol.ext.allow=never")
        .arg("-c")
        .arg("protocol.fd.allow=never")
        .arg("-c")
        .arg("gc.auto=0")
        .arg("-c")
        .arg("maintenance.auto=false")
        .arg("-c")
        .arg(format!("core.hooksPath={}", hooks.display()));
}

struct Ran {
    ok: bool,
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: String,
}

fn run(mut cmd: Command, limit: usize) -> Result<Ran> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.output()?;
    if out.stdout.len() > limit {
        return Err(GitRemoteError::Git {
            stderr: "fatal: asc: git printed more output than allowed".into(),
        });
    }
    let mut stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if stderr.len() > MAX_STDERR {
        let mut cut = MAX_STDERR;
        while !stderr.is_char_boundary(cut) {
            cut -= 1;
        }
        stderr.truncate(cut);
    }
    Ok(Ran {
        ok: out.status.success(),
        code: out.status.code(),
        stdout: out.stdout,
        stderr,
    })
}

fn failed(r: &Ran) -> GitRemoteError {
    let text = r.stderr.trim();
    GitRemoteError::Git {
        stderr: if text.is_empty() {
            format!("fatal: git exited with status {}", r.code.unwrap_or(-1))
        } else {
            text.to_owned()
        },
    }
}

fn parse_oid(hex: &str) -> Option<GitOid> {
    if hex.len() != 40 {
        return None;
    }
    let mut b = [0u8; 20];
    for (i, pair) in hex.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(pair).ok()?;
        b[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(GitOid::from_bytes(b))
}

/// `git ls-remote --symref` against `url`, run outside any repository. Heads and tags only.
pub fn ls_remote(git: &RealGit, url: &str, cwd: &Path) -> Result<Advertised> {
    let url = transport_url(url)?;
    let mut cmd = git.command()?;
    clean_env(&mut cmd);
    safe_args(&mut cmd, &cwd.join("asc-no-hooks"));
    cmd.args([
        "ls-remote",
        "--symref",
        "--",
        &url,
        "HEAD",
        "refs/heads/*",
        "refs/tags/*",
    ])
    .current_dir(cwd);
    let r = run(cmd, MAX_LISTING)?;
    if !r.ok {
        return Err(failed(&r));
    }
    let mut adv = Advertised::default();
    for line in String::from_utf8_lossy(&r.stdout).lines() {
        if let Some(rest) = line.strip_prefix("ref: ") {
            if let Some((target, name)) = rest.split_once('\t')
                && name == "HEAD"
            {
                adv.head = Some(target.to_owned());
            }
            continue;
        }
        let (hex, name) = line
            .split_once('\t')
            .ok_or_else(|| GitRemoteError::BadRef(line.to_owned()))?;
        let oid = parse_oid(hex).ok_or_else(|| GitRemoteError::BadRef(line.to_owned()))?;
        if name == "HEAD" || name.ends_with("^{}") {
            continue;
        }
        if adv.refs.len() >= MAX_REFS {
            return Err(GitRemoteError::TooManyRefs);
        }
        adv.refs.push((name.to_owned(), oid));
    }
    Ok(adv)
}

/// The private bare repository of one asc repository.
#[derive(Clone, Debug)]
pub struct GitRemoteRepo {
    git: RealGit,
    dir: PathBuf,
}

impl GitRemoteRepo {
    /// Opens `dir`, creating the bare repository on first use. `dir` is under asc's own state.
    pub fn open_or_init(git: RealGit, dir: PathBuf) -> Result<Self> {
        let me = Self { git, dir };
        if !(me.dir.join("HEAD").is_file() && me.dir.join("objects").is_dir()) {
            if let Some(parent) = me.dir.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut cmd = me.git.command()?;
            clean_env(&mut cmd);
            cmd.args([
                "init",
                "--bare",
                "--quiet",
                "--template=",
                "--initial-branch=master",
            ])
            .arg("--")
            .arg(&me.dir);
            let r = run(cmd, MAX_LISTING)?;
            if !r.ok {
                return Err(failed(&r));
            }
        }
        Ok(me)
    }

    /// The bare repository's directory (an object database for the importer and exporter).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The real git this repository runs.
    pub fn git(&self) -> &RealGit {
        &self.git
    }

    fn command(&self) -> Result<Command> {
        let mut cmd = self.git.command()?;
        clean_env(&mut cmd);
        safe_args(&mut cmd, &self.dir.join("asc-no-hooks"));
        cmd.arg("--git-dir").arg(&self.dir).current_dir(&self.dir);
        Ok(cmd)
    }

    fn tracked_prefix(ns: &str) -> String {
        format!("refs/ascr/{ns}/")
    }

    /// `git fetch <url>` of every branch and tag into `refs/ascr/<ns>/`, pruning what the remote
    /// dropped. Credentials, `url.<base>.insteadOf` and the transports are real git's.
    pub fn fetch(&self, url: &str, ns: &str) -> Result<()> {
        let url = transport_url(url)?;
        let p = Self::tracked_prefix(ns);
        let mut cmd = self.command()?;
        cmd.args([
            "fetch",
            "--quiet",
            "--no-tags",
            "--prune",
            "--no-write-fetch-head",
            "--no-recurse-submodules",
            "--no-show-forced-updates",
            "--",
        ])
        .arg(&url)
        .arg(format!("+refs/heads/*:{p}heads/*"))
        .arg(format!("+refs/tags/*:{p}tags/*"));
        let r = run(cmd, MAX_LISTING)?;
        if r.ok { Ok(()) } else { Err(failed(&r)) }
    }

    /// `asc clone --reference <git-dir>`: copies the reachable objects of `path` into this
    /// repository (no alternates link), under a scratch ref that is removed afterwards.
    pub fn fetch_reference(&self, path: &Path) -> Result<()> {
        let src = path.to_string_lossy().into_owned();
        if src.starts_with('-') {
            return Err(GitRemoteError::Url(
                "fatal: asc: bad --reference path".into(),
            ));
        }
        let mut cmd = self.command()?;
        cmd.args([
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-recurse-submodules",
            "--",
        ])
        .arg(&src)
        .arg("+refs/heads/*:refs/ascr-reference/heads/*")
        .arg("+refs/tags/*:refs/ascr-reference/tags/*");
        let r = run(cmd, MAX_LISTING)?;
        if !r.ok {
            return Err(failed(&r));
        }
        Ok(())
    }

    /// Drops the scratch refs [`fetch_reference`](Self::fetch_reference) left.
    pub fn drop_reference_refs(&self) -> Result<()> {
        let listed = self.list("refs/ascr-reference/")?;
        let mut input = Vec::new();
        for (name, _) in &listed {
            input.extend_from_slice(format!("delete {name}\0\0").as_bytes());
        }
        self.update_refs_z(&input)
    }

    fn list(&self, prefix: &str) -> Result<Vec<(String, GitOid)>> {
        let mut cmd = self.command()?;
        cmd.args([
            "for-each-ref",
            "--format=%(objectname)%09%(refname)",
            prefix,
        ]);
        let r = run(cmd, MAX_LISTING)?;
        if !r.ok {
            return Err(failed(&r));
        }
        let mut out = Vec::new();
        for line in String::from_utf8_lossy(&r.stdout).lines() {
            let (hex, name) = line
                .split_once('\t')
                .ok_or_else(|| GitRemoteError::BadRef(line.to_owned()))?;
            let oid = parse_oid(hex).ok_or_else(|| GitRemoteError::BadRef(line.to_owned()))?;
            if out.len() >= MAX_REFS {
                return Err(GitRemoteError::TooManyRefs);
            }
            out.push((name.to_owned(), oid));
        }
        Ok(out)
    }

    /// The remote's refs as last fetched, with their ordinary names (`refs/heads/x`,
    /// `refs/tags/x`).
    pub fn tracked(&self, ns: &str) -> Result<Vec<(String, GitOid)>> {
        let p = Self::tracked_prefix(ns);
        Ok(self
            .list(&p)?
            .into_iter()
            .filter_map(|(name, oid)| {
                name.strip_prefix(&p)
                    .map(|rest| (format!("refs/{rest}"), oid))
            })
            .collect())
    }

    /// Moves the tracked copies of pushed refs (`new: None` deletes).
    pub fn set_tracked(&self, ns: &str, updates: &[(String, Option<GitOid>)]) -> Result<()> {
        let p = Self::tracked_prefix(ns);
        let mut input = Vec::new();
        for (dst, new) in updates {
            let Some(rest) = dst.strip_prefix("refs/") else {
                continue;
            };
            if !(rest.starts_with("heads/") || rest.starts_with("tags/"))
                || dst.contains(['\0', '\n'])
            {
                continue;
            }
            match new {
                Some(oid) => {
                    input.extend_from_slice(format!("update {p}{rest}\0{oid}\0\0").as_bytes());
                }
                None => input.extend_from_slice(format!("delete {p}{rest}\0\0").as_bytes()),
            }
        }
        self.update_refs_z(&input)
    }

    fn update_refs_z(&self, input: &[u8]) -> Result<()> {
        if input.is_empty() {
            return Ok(());
        }
        let mut cmd = self.command()?;
        cmd.args(["update-ref", "--stdin", "-z"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            // A failing git closes the pipe early; its status explains why.
            let _ = stdin.write_all(input);
        }
        let out = child.wait_with_output()?;
        if out.status.success() {
            Ok(())
        } else {
            Err(GitRemoteError::Git {
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            })
        }
    }

    /// `git push <url>` of the objects already in this repository, each ref leased
    /// (`--force-with-lease=<ref>:<expected>`). Atomic when several refs move.
    pub fn push(&self, url: &str, refs: &[PushRef]) -> Result<(), PushFailure> {
        let fail = |e: GitRemoteError| PushFailure::Failed(e.to_string());
        let url = transport_url(url).map_err(fail)?;
        let mut cmd = self.command().map_err(fail)?;
        cmd.args([
            "push",
            "--porcelain",
            "--no-verify",
            "--no-recurse-submodules",
        ]);
        if refs.len() > 1 {
            cmd.arg("--atomic");
        }
        let mut specs = Vec::new();
        for r in refs {
            if r.dst.starts_with('-') || r.dst.contains(['\0', '\n', ':', ' ']) {
                return Err(PushFailure::Failed(format!(
                    "fatal: asc: cannot push to the ref name {:?}",
                    r.dst
                )));
            }
            let src = r.new.map(|o| o.to_string()).unwrap_or_default();
            match r.lease {
                Lease::Force => specs.push(format!("+{src}:{}", r.dst)),
                Lease::Absent => {
                    cmd.arg(format!("--force-with-lease={}:", r.dst));
                    specs.push(format!("{src}:{}", r.dst));
                }
                Lease::Value(old) => {
                    cmd.arg(format!("--force-with-lease={}:{old}", r.dst));
                    specs.push(format!("{src}:{}", r.dst));
                }
            }
        }
        cmd.arg("--").arg(&url).args(&specs);
        let ran = run(cmd, MAX_LISTING).map_err(fail)?;
        if ran.ok {
            // Server messages ("remote: ...") reach the user as git shows them.
            for line in ran.stderr.lines().filter(|l| l.starts_with("remote:")) {
                eprintln!("{line}");
            }
            return Ok(());
        }
        for line in String::from_utf8_lossy(&ran.stdout).lines() {
            let Some(rest) = line.strip_prefix('!') else {
                continue;
            };
            let mut parts = rest.trim_start_matches('\t').splitn(3, '\t');
            let (Some(pair), Some(_flag_summary)) = (parts.next(), parts.next()) else {
                continue;
            };
            let reason = parts.next().unwrap_or("");
            let dst = pair.rsplit_once(':').map_or(pair, |(_, d)| d).to_owned();
            let all = format!("{line}{reason}");
            if [
                "stale info",
                "fetch first",
                "non-fast-forward",
                "already exists",
            ]
            .iter()
            .any(|k| all.contains(k))
            {
                return Err(PushFailure::Stale {
                    dst,
                    reason: reason.trim().to_owned(),
                });
            }
        }
        Err(PushFailure::Failed(failed(&ran).to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_lose_the_git_plus_prefix_and_helpers_are_refused() {
        assert_eq!(
            transport_url("git+https://h/o/r.git").unwrap(),
            "https://h/o/r.git"
        );
        assert_eq!(transport_url("git@h:o/r.git").unwrap(), "git@h:o/r.git");
        assert_eq!(transport_url("C:/repos/r.git").unwrap(), "C:/repos/r.git");
        assert!(transport_url("ext::sh -c id").is_err());
        assert!(transport_url("fd::3").is_err());
        assert!(transport_url("-oProxyCommand=x").is_err());
        assert!(transport_url("").is_err());
        assert!(transport_url("a\nb").is_err());
    }

    #[test]
    fn namespaces_are_ref_safe() {
        assert_eq!(namespace("origin"), "origin");
        assert!(namespace("a/b..c").starts_with('x'));
        assert!(!namespace("a b").contains(' '));
    }
}
