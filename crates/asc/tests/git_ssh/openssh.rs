//! Real OpenSSH against the real `asc serve`: key types, the agent, `~/.ssh/config`, URL forms,
//! `asc` over the same URLs, the refusals of SPEC §25.11.4, host keys, and the "never weaken
//! host-key checking" rules of SPEC §15.6.1 and §15.6.4 item 8.
//!
//! This file, `main.rs` and `migrate.rs` are the only files `scripts/ci/no-ssh-weakening.sh`
//! allows to name the host-key options: they assert their absence from what asc runs.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::h::*;
use crate::need_tools;
use crate::support::{Sandbox, text};

const IDENT: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

/// Runs a command with a deadline: a child that hangs (a prompt) fails the test.
fn output_within(mut cmd: Command, secs: u64) -> Output {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let end = Instant::now() + Duration::from_secs(secs);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > end {
            let _ = child.kill();
            panic!("the command did not finish within {secs}s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child.wait_with_output().unwrap()
}

/// `asc <args>` in `cwd` with the sandbox's isolated home, `ssh` as `GIT_SSH_COMMAND` (when
/// given) and `env`.
fn asc(sb: &Sandbox, cwd: &Path, ssh: Option<&str>, env: &[(&str, &str)], args: &[&str]) -> Output {
    let mut cmd = sb.asc(cwd);
    cmd.envs(IDENT).env_remove("SSH_AUTH_SOCK");
    if let Some(s) = ssh {
        cmd.env("GIT_SSH_COMMAND", s);
    }
    cmd.envs(env.iter().copied()).args(args);
    output_within(cmd, 180)
}

fn asc_ok(sb: &Sandbox, cwd: &Path, ssh: &str, args: &[&str]) -> String {
    let out = asc(sb, cwd, Some(ssh), &[], args);
    assert!(
        out.status.success(),
        "asc {args:?} failed: {}",
        mask(&text(&out.stderr))
    );
    text(&out.stdout)
}

fn asc_commit(sb: &Sandbox, w: &Path, ssh: &str, file: &str, body: &str) {
    std::fs::write(w.join(file), body).unwrap();
    asc_ok(sb, w, ssh, &["add", file]);
    asc_ok(sb, w, ssh, &["commit", "-q", "-m", file]);
}

// =================================================================================== keys

#[test]
fn every_key_type_the_agent_and_a_config_alias_log_in() {
    let tools = need_tools!();
    let rig = Rig::new(tools.clone());
    let (git, kh) = (&rig.git, rig.stack.known_hosts.clone());
    let src = rig.seed("src", "acme/web", 2);
    let sha = git.head(&src);
    let url = rig.stack.url("acme/web");
    let cwd = git.work("ls");

    for kind in ["ed25519", "ecdsa", "rsa"] {
        let key = tools.key(rig.keys.path(), &format!("k-{kind}"), kind);
        rig.stack.add_user_key("alice", &key);
        let cmd = rig.cmd(&key);
        let c = git.work(&format!("clone-{kind}"));
        git.ssh_ok(&c, &cmd, &["clone", "-q", &url, "w"]);
        assert_eq!(git.head(&c.join("w")), sha, "{kind} key");
        let _ = &cwd;
    }

    // A key held only by ssh-agent: no -i, and the private home has no default key.
    match Agent::start(&tools) {
        Some(agent) => {
            let key = tools.key(rig.keys.path(), "k-agent", "ed25519");
            rig.stack.add_user_key("alice", &key);
            if agent.add(&key) {
                let cmd = tools.command(None, None, &kh, &[]);
                let r = git.run_env(
                    &cwd,
                    &["ls-remote", &url],
                    &[("GIT_SSH_COMMAND", &cmd), ("SSH_AUTH_SOCK", &agent.sock)],
                );
                assert!(r.ok, "agent login: {}", mask(&r.stderr));
                assert!(r.stdout.contains(&sha), "{}", r.stdout);
            } else {
                assert!(!strict(), "ssh-add could not reach the agent");
                eprintln!("skipped: ssh-add cannot reach this platform's agent");
            }
        }
        None => {
            assert!(!strict(), "ssh-agent could not be started");
            eprintln!("skipped: ssh-agent cannot be driven here");
        }
    }

    // A `Host` alias carrying Port and IdentityFile; the command line adds neither. OpenSSH on
    // Unix finds the user config through the passwd entry, not `HOME`, so `-F` names the file.
    let ssh_dir = git.home.path().join(".ssh");
    std::fs::create_dir_all(&ssh_dir).unwrap();
    let config = ssh_dir.join("config");
    std::fs::write(
        &config,
        format!(
            "Host ascalias\n  HostName 127.0.0.1\n  Port {}\n  User git\n  IdentityFile {}\n  IdentitiesOnly yes\n",
            rig.stack.ssh_port,
            fwd(&rig.alice.path)
        ),
    )
    .unwrap();
    let cmd = tools.command(None, None, &kh, &[&format!("-F \"{}\"", fwd(&config))]);
    let c = git.work("alias");
    git.ssh_ok(&c, &cmd, &["clone", "-q", "git@ascalias:acme/web.git", "w"]);
    assert_eq!(git.head(&c.join("w")), sha);
}

// =================================================================================== URLs and asc

#[test]
fn github_style_and_ssh_urls_work_for_git_and_for_asc() {
    let tools = need_tools!();
    let rig = Rig::new(tools);
    let git = &rig.git;
    let src = rig.seed("src", "acme/web", 2);
    let sha = git.head(&src);

    // git: scp-style (port from the command) and ssh:// (port in the URL), with and without .git.
    let scp = rig.stack.scp_url("acme/web");
    let c = git.work("scp");
    git.ssh_ok(&c, &rig.cmd_p(&rig.alice), &["clone", "-q", &scp, "w"]);
    assert_eq!(git.head(&c.join("w")), sha);
    let bare = format!("ssh://git@127.0.0.1:{}/acme/web", rig.stack.ssh_port);
    let c = git.work("bare");
    git.ssh_ok(&c, &rig.cmd(&rig.alice), &["clone", "-q", &bare, "w"]);
    assert_eq!(git.head(&c.join("w")), sha);

    // asc in git-remote mode, over the same URLs.
    for (name, url, cmd) in [
        ("asc-ssh", rig.stack.url("acme/web"), rig.cmd(&rig.alice)),
        ("asc-scp", scp, rig.cmd_p(&rig.alice)),
    ] {
        let tip = git.ssh_ok(
            &git.work("ls"),
            &cmd,
            &["ls-remote", &url, "refs/heads/master"],
        );
        let tip = tip.split_whitespace().next().unwrap().to_owned();
        let sb = Sandbox::new();
        let dir = sb.dir(name);
        asc_ok(&sb, &dir, &cmd, &["clone", "-q", &url, "w"]);
        let w = dir.join("w");
        assert_eq!(
            text(&asc(&sb, &w, Some(&cmd), &[], &["rev-parse", "HEAD"]).stdout).trim(),
            tip,
            "{name}: asc clone"
        );
        asc_commit(&sb, &w, &cmd, &format!("{name}.txt"), "one\n");
        asc_ok(&sb, &w, &cmd, &["push", "origin", "master"]);
        let pushed = text(&asc(&sb, &w, Some(&cmd), &[], &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_owned();
        let ls = git.ssh_ok(&git.work("ls"), &cmd, &["ls-remote", &url]);
        assert!(
            ls.contains(&format!("{pushed}\trefs/heads/master")),
            "{name}: asc push reached the server: {ls}"
        );
        // A second client fetches it.
        let sb2 = Sandbox::new();
        let dir2 = sb2.dir(name);
        asc_ok(&sb2, &dir2, &cmd, &["clone", "-q", &url, "w"]);
        let w2 = dir2.join("w");
        assert_eq!(
            text(&asc(&sb2, &w2, Some(&cmd), &[], &["rev-parse", "HEAD"]).stdout).trim(),
            pushed
        );
        asc_commit(&sb, &w, &cmd, &format!("{name}-2.txt"), "two\n");
        asc_ok(&sb, &w, &cmd, &["push", "origin", "master"]);
        asc_ok(&sb2, &w2, &cmd, &["fetch", "origin"]);
    }
}

// =================================================================================== refusals

fn ssh_as(rig: &Rig, key: &Key, args: &[&str]) -> Run {
    rig.tools.run_ssh(
        rig.git.home.path(),
        rig.stack.ssh_port,
        Some(key),
        &rig.stack.known_hosts,
        args,
    )
}

#[test]
fn only_the_two_pack_commands_run_and_nothing_is_forwarded() {
    let tools = need_tools!();
    let rig = Rig::new(tools);
    let git = &rig.git;
    rig.seed("src", "acme/web", 1);

    // `ssh -T` is a login test: the greeting on stderr, exit status 1.
    let r = ssh_as(&rig, &rig.alice, &["-T", "git@127.0.0.1"]);
    assert_eq!(r.code, Some(1), "{}", r.stderr);
    assert!(
        r.stderr.contains("successfully authenticated")
            && r.stderr.contains("does not provide shell access"),
        "{}",
        r.stderr
    );
    assert!(r.stdout.is_empty(), "{}", r.stdout);

    // Every other command is refused and nothing it names runs.
    for cmd in [
        "id",
        "sh -c id",
        "git-upload-archive 'acme/web.git'",
        "git upload-pack 'acme/web.git'",
        "scp -t /tmp/x",
        "git-upload-pack 'acme/web.git'; id",
        "git-upload-pack acme/web.git",
    ] {
        let r = ssh_as(&rig, &rig.alice, &["git@127.0.0.1", cmd]);
        assert_eq!(r.code, Some(1), "{cmd}: {}", r.stderr);
        assert!(
            r.stderr.contains("asc: command not allowed"),
            "{cmd}: {}",
            r.stderr
        );
        assert!(r.stdout.is_empty(), "{cmd} produced output: {}", r.stdout);
    }
    let r = ssh_as(&rig, &rig.alice, &["-s", "git@127.0.0.1", "sftp"]);
    assert!(!r.ok && r.stdout.is_empty(), "sftp: {}", r.stderr);

    // The flags change nothing about the answers.
    for flag in ["-A", "-X", "-N"] {
        let args = if flag == "-N" {
            vec![
                "-N",
                "-o",
                "ExitOnForwardFailure=yes",
                "-R",
                "127.0.0.1:0:127.0.0.1:1",
                "git@127.0.0.1",
            ]
        } else {
            vec![flag, "git@127.0.0.1", "id"]
        };
        let r = ssh_as(&rig, &rig.alice, &args);
        assert!(!r.ok, "{flag}: {}", r.stderr);
        assert!(r.stdout.is_empty(), "{flag}: {}", r.stdout);
    }
    let r = ssh_as(
        &rig,
        &rig.alice,
        &[
            "-N",
            "-o",
            "ExitOnForwardFailure=yes",
            "-R",
            "127.0.0.1:0:127.0.0.1:1",
            "git@127.0.0.1",
        ],
    );
    assert!(
        r.stderr.contains("forwarding failed") || r.stderr.contains("Could not request"),
        "remote forwarding must be refused: {}",
        r.stderr
    );

    // -L and -D: the listener opens locally, the channel is refused by the server.
    let local = free_port();
    let target = rig.stack.port;
    let (mut child, err) = hold_ssh(
        &rig,
        &[
            "-N",
            "-L",
            &format!("127.0.0.1:{local}:127.0.0.1:{target}"),
            "git@127.0.0.1",
        ],
        local,
    );
    let mut s = TcpStream::connect(("127.0.0.1", local)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let _ = s.write_all(b"GET /healthz HTTP/1.0\r\n\r\n");
    let mut got = Vec::new();
    let _ = s.read_to_end(&mut got);
    assert!(
        !String::from_utf8_lossy(&got).contains("HTTP/"),
        "the forwarded port reached the server's HTTP port"
    );
    drop(s);
    std::thread::sleep(Duration::from_millis(300));
    let _ = child.kill();
    let _ = child.wait();
    let said = std::fs::read_to_string(&err).unwrap_or_default();
    assert!(said.contains("administratively prohibited"), "-L: {said}");

    let socks = free_port();
    let (mut child, _err) = hold_ssh(
        &rig,
        &["-N", "-D", &format!("127.0.0.1:{socks}"), "git@127.0.0.1"],
        socks,
    );
    let mut s = TcpStream::connect(("127.0.0.1", socks)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    s.write_all(&[5, 1, 0]).unwrap();
    let mut hello = [0u8; 2];
    s.read_exact(&mut hello).unwrap();
    let mut req = vec![5, 1, 0, 1, 127, 0, 0, 1];
    req.extend_from_slice(&target.to_be_bytes());
    s.write_all(&req).unwrap();
    let mut reply = [0u8; 10];
    let n = s.read(&mut reply).unwrap_or(0);
    // ssh answers a failed channel open with a SOCKS failure code or just closes.
    assert!(
        !(n >= 2 && reply[1] == 0),
        "-D: the SOCKS connect must not succeed: {reply:?}"
    );
    let _ = child.kill();
    let _ = child.wait();

    // The server is unaffected.
    let ls = git.ssh_ok(
        &git.work("ls"),
        &rig.cmd(&rig.alice),
        &["ls-remote", &rig.stack.url("acme/web")],
    );
    assert!(ls.contains("refs/heads/master"), "{ls}");
}

/// Starts a long-running `ssh` and waits until its local listener on `port` accepts.
fn hold_ssh(rig: &Rig, args: &[&str], port: u16) -> (std::process::Child, PathBuf) {
    let err = rig.git.home.path().join(format!("hold-{port}.err"));
    let mut cmd = Command::new(&rig.tools.ssh);
    cmd.args(["-p", &rig.stack.ssh_port.to_string(), "-i"])
        .arg(&rig.alice.path)
        .args(["-o", "IdentitiesOnly=yes"])
        .arg("-o")
        .arg(format!(
            "UserKnownHostsFile={}",
            fwd(&rig.stack.known_hosts)
        ))
        .args(["-o", "ExitOnForwardFailure=yes"])
        .args(args)
        .env("HOME", rig.git.home.path())
        .env("USERPROFILE", rig.git.home.path())
        .env_remove("SSH_AUTH_SOCK")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&err).unwrap());
    let mut child = cmd.spawn().unwrap();
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "ssh exited early: {}",
            std::fs::read_to_string(&err).unwrap_or_default()
        );
        assert!(Instant::now() < end, "the local forward never listened");
        std::thread::sleep(Duration::from_millis(100));
    }
    (child, err)
}

// =================================================================================== denied logins

#[test]
fn unknown_revoked_and_misnamed_keys_and_deploy_keys_are_refused() {
    let tools = need_tools!();
    let rig = Rig::new(tools.clone());
    let git = &rig.git;
    let src = rig.seed("src", "acme/web", 1);
    rig.seed("src2", "acme/lib", 1);
    let sha = git.head(&src);
    let url = rig.stack.url("acme/web");
    let cwd = git.work("ls");
    let denied = |r: &Run, what: &str| {
        assert_eq!(r.code, Some(128), "{what}: {}", r.stderr);
        assert!(
            r.stderr.contains("Permission denied (publickey)"),
            "{what}: {}",
            mask(&r.stderr)
        );
    };

    let stranger = tools.key(rig.keys.path(), "stranger", "ed25519");
    denied(
        &git.ssh(&cwd, &rig.cmd(&stranger), &["ls-remote", &url]),
        "unregistered key",
    );

    let revoked = tools.key(rig.keys.path(), "revoked", "ed25519");
    rig.stack.add_user_key("alice", &revoked);
    assert!(git.ssh(&cwd, &rig.cmd(&revoked), &["ls-remote", &url]).ok);
    let revoked2 = tools.key(rig.keys.path(), "revoked2", "ed25519");
    rig.stack.add_user_key("alice", &revoked2);
    rig.stack.revoke_key(&revoked2);
    denied(
        &git.ssh(&cwd, &rig.cmd(&revoked2), &["ls-remote", &url]),
        "revoked key",
    );

    let wrong_user = format!("ssh://other@127.0.0.1:{}/acme/web.git", rig.stack.ssh_port);
    denied(
        &git.ssh(&cwd, &rig.cmd(&rig.alice), &["ls-remote", &wrong_user]),
        "wrong user name",
    );

    // Deploy keys reach their one repository only.
    let ro = tools.key(rig.keys.path(), "deploy-ro", "ed25519");
    let rw = tools.key(rig.keys.path(), "deploy-rw", "ecdsa");
    rig.stack.add_deploy_key("acme/web", false, &ro);
    rig.stack.add_deploy_key("acme/web", true, &rw);
    assert!(git.ssh(&cwd, &rig.cmd(&ro), &["ls-remote", &url]).ok);
    let lib = rig.stack.url("acme/lib");
    let r = git.ssh(&cwd, &rig.cmd(&ro), &["ls-remote", &lib]);
    assert_eq!(r.code, Some(128), "{}", r.stderr);
    assert!(
        r.stderr.contains("repository not found"),
        "a deploy key on the wrong repo: {}",
        mask(&r.stderr)
    );
    let missing = rig.stack.url("acme/nothing");
    let r2 = git.ssh(&cwd, &rig.cmd(&ro), &["ls-remote", &missing]);
    assert_eq!(
        r.stderr
            .lines()
            .filter(|l| l.contains("repository not found"))
            .count(),
        r2.stderr
            .lines()
            .filter(|l| l.contains("repository not found"))
            .count(),
        "a missing and an unreadable repository look the same"
    );

    let c = git.work("deploy");
    git.ssh_ok(&c, &rig.cmd(&ro), &["clone", "-q", &url, "w"]);
    assert_eq!(git.head(&c.join("w")), sha);
    git.commit(&c.join("w"), "d.txt", "d\n", "d");
    let r = git.ssh(&c.join("w"), &rig.cmd(&ro), &["push", "origin", "master"]);
    assert!(
        !r.ok && r.stderr.contains("read-only access"),
        "{}",
        mask(&r.stderr)
    );
    let r = git.ssh(&c.join("w"), &rig.cmd(&rw), &["push", "origin", "master"]);
    assert!(r.ok, "a write deploy key pushes: {}", mask(&r.stderr));

    // Nothing above made the server fall over.
    assert!(git.ssh(&cwd, &rig.cmd(&rig.alice), &["ls-remote", &url]).ok);
}

// =================================================================================== host keys

#[test]
fn a_changed_or_unknown_host_key_fails_for_git_and_for_asc() {
    let tools = need_tools!();
    let rig = Rig::new(tools.clone());
    let git = &rig.git;
    rig.seed("src", "acme/web", 1);
    let url = rig.stack.url("acme/web");
    let good = rig.cmd(&rig.alice);
    let cwd = git.work("ls");

    // A working asc clone first, with the right host key.
    let sb = Sandbox::new();
    let dir = sb.dir("c");
    asc_ok(&sb, &dir, &good, &["clone", "-q", &url, "w"]);
    let w = dir.join("w");

    // The pinned key is another key: the real ssh says so and neither client overrides it.
    let other = tools.key(rig.keys.path(), "other-host", "ed25519");
    let mut parts = other.line();
    parts = parts
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    let wrong = format!("[127.0.0.1]:{} {parts}\n", rig.stack.ssh_port);
    std::fs::write(&rig.stack.known_hosts, &wrong).unwrap();
    let r = git.ssh(&cwd, &good, &["ls-remote", &url]);
    assert_eq!(r.code, Some(128), "{}", r.stderr);
    assert!(
        r.stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED"),
        "git: {}",
        mask(&r.stderr)
    );
    let out = asc(&sb, &w, Some(&good), &[], &["fetch", "origin"]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("REMOTE HOST IDENTIFICATION HAS CHANGED"),
        "asc fetch: {}",
        mask(&text(&out.stderr))
    );
    assert_eq!(
        std::fs::read_to_string(&rig.stack.known_hosts).unwrap(),
        wrong,
        "no client rewrote the pinned key"
    );

    // An unknown host with BatchMode: no question is asked, the connection is refused.
    std::fs::write(&rig.stack.known_hosts, "").unwrap();
    let batch = rig.tools.command(
        None,
        Some(&rig.alice),
        &rig.stack.known_hosts,
        &["-o", "BatchMode=yes"],
    );
    let r = git.ssh(&cwd, &batch, &["ls-remote", &url]);
    assert_eq!(r.code, Some(128), "{}", r.stderr);
    assert!(
        r.stderr.contains("Host key verification failed"),
        "{}",
        r.stderr
    );

    // Under Prompt::Never asc's child ssh has no terminal to ask on: it fails, it does not hang
    // and it does not trust the key.
    let out = asc(&sb, &w, Some(&good), &[], &["fetch", "origin"]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("Host key verification failed"),
        "{}",
        mask(&text(&out.stderr))
    );
    assert_eq!(
        std::fs::read_to_string(&rig.stack.known_hosts).unwrap(),
        "",
        "the host key was not added"
    );
}

// =================================================================================== no weakening

/// The shell program that records its argv and fails, in place of ssh.
fn recorder(dir: &Path, name: &str) -> (String, PathBuf) {
    let script = dir.join(format!("{name}.sh"));
    let log = dir.join(format!("{name}.log"));
    std::fs::write(
        &script,
        "#!/bin/sh\n{ for a in \"$@\"; do printf '%s\\n' \"$a\"; done; echo '--end'; } >> \"$REC\"\nexit 255\n",
    )
    .unwrap();
    (format!("sh \"{}\"", fwd(&script)), log)
}

fn blocks(log: &Path) -> Vec<Vec<String>> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let mut out = vec![Vec::new()];
    for l in text.lines() {
        if l == "--end" {
            out.push(Vec::new());
        } else {
            out.last_mut().unwrap().push(l.to_owned());
        }
    }
    out.pop();
    out
}

/// The option words no client of ours may add (SPEC §15.6.1).
const FORBIDDEN: [&str; 4] = [
    "StrictHostKeyChecking",
    "UserKnownHostsFile",
    "CheckHostIP",
    "VerifyHostKeyDNS",
];

#[test]
fn asc_starts_ssh_with_the_argv_git_itself_produces() {
    let tools = need_tools!();
    let rig = Rig::new(tools);
    let git = &rig.git;
    let url = rig.stack.url("acme/web");
    let dir = tempfile::tempdir().unwrap();
    let (cmd, log) = recorder(dir.path(), "rec");
    let rec = fwd(&log);
    let env = [
        ("GIT_SSH_COMMAND", cmd.as_str()),
        ("GIT_SSH_VARIANT", "ssh"),
        ("REC", rec.as_str()),
    ];

    // git's own argv for the URL.
    let r = git.run_env(&git.work("ls"), &["ls-remote", &url], &env);
    assert!(!r.ok, "the recorder fails the connection");
    let git_blocks = blocks(&log);
    assert!(!git_blocks.is_empty(), "git ran the command: {}", r.stderr);
    std::fs::remove_file(&log).unwrap();

    // asc's: the first ssh it starts has the same argv.
    let sb = Sandbox::new();
    let d = sb.dir("c");
    let out = asc(&sb, &d, None, &env, &["clone", "-q", &url, "w"]);
    assert!(!out.status.success());
    let asc_blocks = blocks(&log);
    assert!(
        !asc_blocks.is_empty(),
        "asc ran the command: {}",
        text(&out.stderr)
    );
    assert_eq!(asc_blocks[0], git_blocks[0], "asc's ssh argv equals git's");

    for block in git_blocks.iter().chain(asc_blocks.iter()) {
        for (i, a) in block.iter().enumerate() {
            assert!(
                !FORBIDDEN.iter().any(|f| a.contains(f)),
                "a host-key option was added: {a}"
            );
            if a == "-o" {
                assert_eq!(
                    block.get(i + 1).map(String::as_str),
                    Some("SendEnv=GIT_PROTOCOL"),
                    "the only -o git adds"
                );
            }
        }
    }
}

#[test]
fn ssh_command_settings_from_trusted_config_are_honoured() {
    let tools = need_tools!();
    let rig = Rig::new(tools);
    let url = rig.stack.url("acme/web");
    let dir = tempfile::tempdir().unwrap();
    let (cfg_cmd, cfg_log) = recorder(dir.path(), "from-config");
    let (env_cmd, env_log) = recorder(dir.path(), "from-env");

    // core.sshCommand in the user's global config.
    let sb = Sandbox::new();
    std::fs::write(
        sb.user.join(".gitconfig"),
        format!("[core]\n\tsshCommand = {}\n", cfg_cmd.replace('"', "\\\"")),
    )
    .unwrap();
    let d = sb.dir("c");
    let rec = fwd(&cfg_log);
    let out = asc(
        &sb,
        &d,
        None,
        &[("REC", &rec), ("GIT_SSH_VARIANT", "ssh")],
        &["clone", "-q", &url, "w"],
    );
    assert!(!out.status.success());
    assert!(
        !blocks(&cfg_log).is_empty(),
        "core.sshCommand was run: {}",
        text(&out.stderr)
    );

    // GIT_SSH_COMMAND wins over it, as in git.
    let rec2 = fwd(&env_log);
    let out = asc(
        &sb,
        &d,
        Some(&env_cmd),
        &[("REC", &rec2), ("GIT_SSH_VARIANT", "ssh")],
        &["clone", "-q", &url, "w2"],
    );
    assert!(!out.status.success());
    assert!(
        !blocks(&env_log).is_empty(),
        "GIT_SSH_COMMAND was run: {}",
        text(&out.stderr)
    );
    // `core.sshCommand` in a repository config owned by another user is dropped by the
    // reduced-trust rules; that gate is tested with the config module's own ownership cases
    // (`cli::config`, SPEC §15.6.4 item 2), since a foreign owner cannot be made here.
}

// =================================================================================== no secrets

#[test]
fn logs_hold_no_key_material() {
    let tools = need_tools!();
    let rig = Rig::new(tools);
    rig.seed("src", "acme/web", 1);
    let git = &rig.git;
    // A refused login and a refused command leave records too.
    let stranger = rig.tools.key(rig.keys.path(), "stranger", "ed25519");
    let _ = git.ssh(
        &git.work("ls"),
        &rig.cmd(&stranger),
        &["ls-remote", &rig.stack.url("acme/web")],
    );
    let _ = ssh_as(&rig, &rig.alice, &["git@127.0.0.1", "id"]);
    std::thread::sleep(Duration::from_millis(500));
    // What the server process and its operator commands print.
    let mut log = rig.stack.log();
    for args in [&["ssh-key", "list"][..], &["ssh", "host-key", "show"][..]] {
        let out = rig.stack.serve(args);
        assert!(out.status.success(), "{args:?}: {}", lossy(&out.stderr));
        log.push_str(&lossy(&out.stdout));
        log.push_str(&lossy(&out.stderr));
    }
    assert!(
        log.contains("SHA256:"),
        "keys are named by fingerprint: {log}"
    );
    for key in [&rig.alice, &rig.bob, &stranger] {
        assert!(
            !log.contains(&key.private_body()),
            "private key bytes in the log"
        );
        assert!(!log.contains(&key.blob()), "a public key blob in the log");
    }
    let host_key =
        std::fs::read_to_string(rig.stack.dir.path().join("ssh_host_ed25519_key")).unwrap();
    for line in host_key
        .lines()
        .filter(|l| l.len() > 20 && !l.starts_with("-----"))
    {
        assert!(!log.contains(line), "host key bytes in the log");
    }
}

// =================================================================================== the CI script

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn the_no_weakening_script_passes_here_and_fails_on_a_planted_option() {
    let script = repo_root().join("scripts/ci/no-ssh-weakening.sh");
    let run = |root: Option<&Path>| {
        let mut cmd = Command::new("sh");
        cmd.arg(&script).current_dir(repo_root());
        if let Some(r) = root {
            cmd.env("NO_SSH_WEAKENING_ROOT", r);
        }
        output_within(cmd, 120)
    };
    let ok = run(None);
    assert!(ok.status.success(), "{}", text(&ok.stderr));

    for word in FORBIDDEN {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("crates/x/src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("lib.rs"),
            format!("const A: &str = \"-o {word}=no\";\n"),
        )
        .unwrap();
        let bad = run(Some(dir.path()));
        assert_eq!(bad.status.code(), Some(1), "{word}: {}", text(&bad.stderr));
        assert!(text(&bad.stderr).contains(word), "{}", text(&bad.stderr));
    }
}
