//! The `pre-receive` hook protocol against real git >= 2.40 (U82, SPEC §25.2, §25.8): what git
//! hands the hook, what `ServerGit` lets a caller add to the hook's environment, and what the
//! server reads from a push's quarantine.
#![cfg(feature = "git-compat")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use asc_core::id::GitOid;
use asc_git::compat::hook::{ENV_ENDPOINT, ENV_TOKEN, HookInput};
use asc_git::compat::receive::{PushOptions, check_stats, validate_quarantine};
use asc_git::compat::{
    FsckMode, GitLimits, PackRequest, PackService, QuarantineStats, ServerGit, ServerGitConfig,
    ServerGitError,
};
use asc_git::realgit::RealGit;

const ZERO: &str = "0000000000000000000000000000000000000000";
const TOKEN: &str = "ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12";

fn real_git() -> RealGit {
    RealGit::resolve(None, None).expect("real git >= 2.40")
}

fn fwd(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
    let mut cmd = real_git().command().expect("git");
    cmd.current_dir(dir)
        .args(["-c", "user.name=A", "-c", "user.email=a@x"])
        .args(args);
    let out = cmd.output().expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn pkt(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(format!("{:04x}", data.len() + 4).as_bytes());
    out.extend_from_slice(data);
}

/// A client repository with one commit holding `blob_bytes` of data; returns the repository,
/// the commit id and the request body of a push of it to `refs/heads/topic` with `options`.
fn push_body(dir: &Path, blob_bytes: usize, options: &[&str]) -> (PathBuf, String, Vec<u8>) {
    let repo = dir.join("client");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "--initial-branch=master"]);
    let data: Vec<u8> = (0..blob_bytes)
        .map(|i| u8::try_from((i * 31 + i / 7) % 251).unwrap())
        .collect();
    fs::write(repo.join("big.bin"), data).unwrap();
    git(&repo, &["add", "big.bin"]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    let commit = String::from_utf8(git(&repo, &["rev-parse", "HEAD"]))
        .unwrap()
        .trim()
        .to_owned();
    let mut body = Vec::new();
    let caps = if options.is_empty() {
        "report-status side-band-64k"
    } else {
        "report-status side-band-64k push-options"
    };
    pkt(
        &mut body,
        format!("{ZERO} {commit} refs/heads/topic\0 {caps} agent=test\n").as_bytes(),
    );
    body.extend_from_slice(b"0000");
    if !options.is_empty() {
        for o in options {
            pkt(&mut body, o.as_bytes());
        }
        body.extend_from_slice(b"0000");
    }
    let mut cmd = real_git().command().unwrap();
    cmd.current_dir(&repo)
        .args(["pack-objects", "--stdout", "--revs", "-q"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{commit}\n").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    body.extend_from_slice(&out.stdout);
    (repo, commit, body)
}

struct Env {
    dir: tempfile::TempDir,
    server: ServerGit,
    view: PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let server = ServerGit::new(
            &real_git(),
            ServerGitConfig {
                hooks_dir: dir.path().join("hooks"),
                state_dir: dir.path().join("state"),
                limits: GitLimits::default(),
                fsck: FsckMode::Strict,
            },
        )
        .unwrap();
        let view = dir.path().join("view.git");
        server.init_bare(&view).unwrap();
        Self { dir, server, view }
    }

    /// Installs a `pre-receive` that records git's stdin and environment, then waits for the
    /// file `go` before exiting `code` (so the quarantine can be inspected meanwhile).
    fn capture_hook(&self, code: i32, message: &str) -> PathBuf {
        let cap = self.dir.path().join("capture");
        fs::create_dir_all(&cap).unwrap();
        let script = format!(
            "#!/bin/sh\ncat > \"$ASC_CAPTURE/stdin\"\nenv > \"$ASC_CAPTURE/env\"\n\
             : > \"$ASC_CAPTURE/ready\"\n\
             while [ ! -f \"$ASC_CAPTURE/go\" ]; do sleep 0.05; done\n\
             echo '{message}' >&2\nexit {code}\n"
        );
        let hook = self.dir.path().join("hooks").join("pre-receive");
        fs::write(&hook, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        }
        cap
    }
}

/// Runs a push through `receive-pack --stateless-rpc` with the capture hook. `inspect` runs while
/// the hook waits; returns git's response.
fn run_push(env: &Env, body: Vec<u8>, cap: &Path, inspect: impl FnOnce(&Path)) -> Vec<u8> {
    let extra = [
        ("ASC_CAPTURE", fwd(cap)),
        (ENV_TOKEN, TOKEN.to_owned()),
        (ENV_ENDPOINT, "asc-test-endpoint".to_owned()),
    ];
    let extra_ref: Vec<(&str, &str)> = extra.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut child = env
        .server
        .pack_with_env(
            PackService::ReceivePack,
            &env.view,
            &PackRequest::default(),
            &extra_ref,
        )
        .expect("spawn receive-pack");
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&body);
    });
    let started = Instant::now();
    while !cap.join("ready").exists() {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the hook never ran"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    inspect(cap);
    fs::write(cap.join("go"), b"").unwrap();
    let mut out = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut out).unwrap();
    let _ = writer.join();
    let _ = child.wait();
    out
}

fn captured_env(cap: &Path) -> Vec<(OsString, OsString)> {
    fs::read_to_string(cap.join("env"))
        .unwrap()
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect()
}

#[test]
fn git_hands_the_hook_its_commands_options_and_quarantine() {
    let env = Env::new();
    let cap = env.capture_hook(0, "all good");
    let (_client, commit, body) = push_body(env.dir.path(), 4096, &["asc.ttl=1d", "topic=t"]);
    let mut seen = None;
    let out = run_push(&env, body, &cap, |cap| {
        let stdin = fs::read(cap.join("stdin")).unwrap();
        let input = HookInput::collect(stdin.as_slice(), captured_env(cap)).unwrap();
        // The quarantine git made is accepted for this view and for no other.
        let q = validate_quarantine(&env.view, &input.request)
            .expect("git's own quarantine")
            .expect("a push of objects has a quarantine");
        assert!(q.object_dir.to_string_lossy().contains("incoming-"));
        let other = env.dir.path().join("other.git");
        env.server.init_bare(&other).unwrap();
        assert!(validate_quarantine(&other, &input.request).is_err());
        // Three objects: the commit, its tree and the blob.
        let stats = env
            .server
            .quarantine_stats(&env.view, &q.object_dir)
            .unwrap();
        assert_eq!(stats.objects, 3);
        assert_eq!(stats.largest, 4096);
        seen = Some(input);
    });
    let input = seen.expect("the hook ran");
    let cmds = &input.request.commands;
    assert_eq!(cmds.len(), 1);
    assert_eq!(cmds[0].name, "refs/heads/topic");
    assert_eq!(cmds[0].old, None);
    assert_eq!(cmds[0].new, Some(commit.parse::<GitOid>().unwrap()));
    assert_eq!(input.endpoint, "asc-test-endpoint");
    assert_eq!(input.request.token, TOKEN);
    let opts = PushOptions::from_request(&input.request).unwrap();
    assert_eq!(opts.all, ["asc.ttl=1d", "topic=t"]);
    assert_eq!(opts.ttl_seconds, 86_400);
    // The hook accepted: git reports the ref as updated and applies it to the view.
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains("unpack ok") && text.contains("ok refs/heads/topic"),
        "{text}"
    );
    let refs = env.server.list_refs(&env.view).unwrap();
    assert_eq!(refs.len(), 1);
}

#[test]
fn a_hook_refusal_reaches_the_client_as_remote_lines_and_changes_nothing() {
    let env = Env::new();
    let cap = env.capture_hook(1, "asc: not today");
    let (_client, _commit, body) = push_body(env.dir.path(), 64, &[]);
    let out = run_push(&env, body, &cap, |_| {});
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("asc: not today"), "{text}");
    assert!(text.contains("pre-receive hook declined"), "{text}");
    assert!(text.contains("ng refs/heads/topic"), "{text}");
    assert!(env.server.list_refs(&env.view).unwrap().is_empty());
}

#[test]
fn the_hook_environment_is_git_plus_the_asc_variables_and_nothing_of_the_parent() {
    let env = Env::new();
    let cap = env.capture_hook(0, "");
    let (_client, _commit, body) = push_body(env.dir.path(), 64, &[]);
    run_push(&env, body, &cap, |_| {});
    let names: Vec<String> = captured_env(&cap)
        .into_iter()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    // The two hook variables arrive; no credential-shaped variable of the parent does.
    assert!(names.iter().any(|n| n == ENV_TOKEN));
    assert!(names.iter().any(|n| n == ENV_ENDPOINT));
    for n in &names {
        let upper = n.to_ascii_uppercase();
        assert!(
            ![
                "TOKEN",
                "SECRET",
                "PASSWORD",
                "PASSWD",
                "CREDENTIAL",
                "AUTHORIZATION"
            ]
            .iter()
            .any(|w| upper.contains(w))
                || n == ENV_TOKEN,
            "{n} reached the hook"
        );
    }
    assert!(!names.iter().any(|n| n.starts_with("GIT_CONFIG_KEY_")));
}

#[test]
fn extra_environment_names_cannot_reach_git_switches() {
    let env = Env::new();
    for (name, value) in [
        ("GIT_CONFIG_COUNT", "1"),
        ("GIT_DIR", "."),
        ("GIT_EXEC_PATH", "/x"),
        ("HOME", "/x"),
        ("PATH", "/x"),
        ("asc_lower", "x"),
        ("ASC_BAD-NAME", "x"),
        ("ASC_NUL", "a\0b"),
    ] {
        let err = env
            .server
            .pack_with_env(
                PackService::ReceivePack,
                &env.view,
                &PackRequest::default(),
                &[(name, value)],
            )
            .err()
            .unwrap_or_else(|| panic!("{name} was accepted"));
        assert!(matches!(err, ServerGitError::Config(_)), "{name}: {err}");
    }
}

#[test]
fn the_quarantine_limits_refuse_a_push_before_import() {
    let env = Env::new();
    let cap = env.capture_hook(0, "");
    let (_client, _commit, body) = push_body(env.dir.path(), 200_000, &[]);
    let limits = GitLimits {
        max_object_bytes: 100_000,
        max_push_bytes: 1 << 30,
        ..GitLimits::default()
    };
    let mut verdict = None;
    run_push(&env, body, &cap, |cap| {
        let stdin = fs::read(cap.join("stdin")).unwrap();
        let input = HookInput::collect(stdin.as_slice(), captured_env(cap)).unwrap();
        let q = validate_quarantine(&env.view, &input.request)
            .unwrap()
            .unwrap();
        let stats = env
            .server
            .quarantine_stats(&env.view, &q.object_dir)
            .unwrap();
        verdict = Some((stats, check_stats(&stats, &limits)));
    });
    let (stats, verdict) = verdict.unwrap();
    assert_eq!(stats.largest, 200_000);
    let text = verdict.unwrap_err().to_string();
    assert_eq!(
        text,
        "asc: push has an object of 200000 bytes, the limit is 100000"
    );
    let few = GitLimits {
        max_push_objects: 2,
        ..GitLimits::default()
    };
    let text = check_stats(
        &QuarantineStats {
            objects: stats.objects,
            largest: 0,
        },
        &few,
    )
    .unwrap_err()
    .to_string();
    assert_eq!(text, "asc: push has 3 objects, the limit is 2");
}
