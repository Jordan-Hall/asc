//! U86: the SSH listener and key authentication (SPEC §25.11.1 to §25.11.4, §25.11.6 to
//! §25.11.8, §25.11.11). A `russh` client drives a real listener on a loopback port; the
//! authorizer, the audit sink and the pack command are recording fakes.
#![cfg(feature = "git-ssh")]

#[path = "support/ssh_harness.rs"]
mod harness;
#[path = "support/ssh_keys.rs"]
mod keys;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use asc_rpc::auth::SshKeyKind;
use asc_serve::config::{Limits, Secs};
use asc_serve::limits::{Caller, Class, Peer};
use asc_serve::ssh::audit::LoginOutcome;
use harness::*;
use keys::*;
use russh::client::AuthResult;
use russh::keys::HashAlg;
use russh::{ChannelOpenFailure, MethodKind, MethodSet};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

const LOCAL: [u8; 4] = [127, 0, 0, 1];

/// What every refused login looks like to the client: only `publickey` is left to try.
fn denied() -> AuthResult {
    AuthResult::Failure {
        remaining_methods: MethodSet::from(&[MethodKind::PublicKey][..]),
        partial_success: false,
    }
}

async fn alice(h: &Harness) -> Conn {
    h.authz.add_user(ED_A_PUBLIC, "alice");
    let mut c = connect(h.addr).await;
    assert_eq!(
        c.login("git", ED_A_PRIVATE, None).await,
        AuthResult::Success
    );
    c
}

// ------------------------------------------------------------------------------ login

#[tokio::test]
async fn a_known_key_logs_in_and_writes_one_audit_record() {
    let h = start().await;
    h.authz
        .add(ED_A_PUBLIC, "key-7", SshKeyKind::User, "pat:alice", None);
    let mut c = connect(h.addr).await;
    assert_eq!(c.host.lock().unwrap().unwrap(), h.host_fingerprint);
    assert_eq!(
        c.login("git", ED_A_PRIVATE, None).await,
        AuthResult::Success
    );

    eventually("login record", || h.records.logins().len() == 1).await;
    let rec = &h.records.logins()[0];
    assert_eq!(rec.outcome, LoginOutcome::Ok);
    assert_eq!(rec.fingerprint, Some(fingerprint(ED_A_PUBLIC)));
    assert_eq!(rec.key_type, "ssh-ed25519");
    assert_eq!(rec.user_hint, "git");
    assert_eq!(rec.principal.as_deref(), Some("pat:alice"));
    assert_eq!(rec.key_kind, Some("user"));
    assert_eq!(rec.key_id.as_deref(), Some("key-7"));
    assert_eq!(rec.peer, Some(LOCAL.into()));
    // No key blob and no private key in anything the audit holds.
    let blob = ED_A_PUBLIC.split_whitespace().nth(1).unwrap();
    let body: Vec<_> = ED_A_PRIVATE
        .lines()
        .filter(|l| !l.starts_with("---"))
        .collect();
    let text = h.records.debug_text();
    assert!(!text.contains(blob), "public key blob in the audit records");
    for line in body.iter().map(|l| l.trim()).filter(|l| !l.is_empty()) {
        assert!(
            !text.contains(line),
            "private key text in the audit records"
        );
    }
    assert!(text.contains("SHA256:"));
    assert_eq!(h.metrics.logins_total("ok"), 1);
    eventually("session gauge", || h.metrics.sessions() == 1).await;
    drop(c);
    eventually("session gauge", || h.metrics.sessions() == 0).await;
}

#[tokio::test]
async fn every_refusal_looks_the_same_to_the_client() {
    let h = start().await;
    h.authz.add_user(ED_A_PUBLIC, "alice");
    h.authz.add_user(ED_B_PUBLIC, "bob");
    h.authz.add_user(RSA1024_PUBLIC, "weak");
    h.authz.add_user(EC_PUBLIC, "carol");
    h.authz.add_user(ED_B_PUBLIC, "bob");
    h.authz.revoke(ED_B_PUBLIC);

    // The wrong SSH user name, an unknown key, a revoked key and a weak RSA key.
    let cases: [(&str, &str, &str); 4] = [
        ("root", ED_A_PRIVATE, "rejected:user"),
        ("git", ED_C_PRIVATE, "rejected:unknown_key"),
        ("git", ED_B_PRIVATE, "rejected:unknown_key"),
        ("git", RSA1024_PRIVATE, "rejected:rsa_bits"),
    ];
    for (user, key, outcome) in cases {
        let mut c = connect(h.addr).await;
        assert_eq!(
            c.login(user, key, Some(HashAlg::Sha256)).await,
            denied(),
            "{user} {outcome}"
        );
        eventually(outcome, || {
            h.records.outcomes().iter().any(|o| o == outcome)
        })
        .await;
    }
    let outcomes = h.records.outcomes();
    assert_eq!(outcomes.len(), 4, "{outcomes:?}");
    // The audit keeps the user name as a hint, and the fingerprint of the offered key.
    let by = |o: &str| {
        h.records
            .logins()
            .into_iter()
            .find(|r| r.outcome.label() == o)
            .unwrap()
    };
    assert_eq!(by("rejected:user").user_hint, "root");
    assert_eq!(
        by("rejected:unknown_key").fingerprint,
        Some(fingerprint(ED_C_PUBLIC))
    );
    assert_eq!(by("rejected:rsa_bits").key_type, "ssh-rsa");
    assert_eq!(h.metrics.logins_total("rejected:user"), 1);
}

#[tokio::test]
async fn an_authorizer_failure_refuses_the_login() {
    let h = start().await;
    h.authz.add_user(ED_A_PUBLIC, "alice");
    h.authz.fail_identify.store(true, Ordering::SeqCst);
    let mut c = connect(h.addr).await;
    assert_eq!(c.login("git", ED_A_PRIVATE, None).await, denied());
    eventually("record", || {
        h.records.outcomes() == ["rejected:unknown_key"]
    })
    .await;
    // Back up: the same key logs in.
    h.authz.fail_identify.store(false, Ordering::SeqCst);
    let mut c = connect(h.addr).await;
    assert_eq!(
        c.login("git", ED_A_PRIVATE, None).await,
        AuthResult::Success
    );
}

#[tokio::test]
async fn a_key_revoked_before_it_logs_in_is_refused_and_the_user_name_may_be_any_when_unset() {
    let h = start_with(|ssh, _| ssh.user = String::new(), Limits::default()).await;
    h.authz.add_user(ED_A_PUBLIC, "alice");
    let mut c = connect(h.addr).await;
    assert_eq!(
        c.login("anyone", ED_A_PRIVATE, None).await,
        AuthResult::Success
    );
    h.authz.revoke(ED_A_PUBLIC);
    let mut c = connect(h.addr).await;
    assert_eq!(c.login("anyone", ED_A_PRIVATE, None).await, denied());
}

#[tokio::test]
async fn password_keyboard_interactive_and_none_are_refused() {
    let h = start().await;
    h.authz.add_user(ED_A_PUBLIC, "alice");
    let mut c = connect(h.addr).await;
    // `none` is answered with the method list: publickey only.
    let none = within(10, c.handle.authenticate_none("git")).await.unwrap();
    assert_eq!(none, denied());
    let pw = within(10, c.handle.authenticate_password("git", "hunter2"))
        .await
        .unwrap();
    assert_eq!(pw, denied());
    let ki = within(
        10,
        c.handle
            .authenticate_keyboard_interactive_start("git", None::<String>),
    )
    .await
    .unwrap();
    assert!(
        matches!(
            ki,
            russh::client::KeyboardInteractiveAuthResponse::Failure { .. }
        ),
        "{ki:?}"
    );
    eventually("method records", || {
        h.records
            .outcomes()
            .iter()
            .filter(|o| *o == "rejected:method")
            .count()
            == 2
    })
    .await;
    // The password is nowhere in what was recorded.
    assert!(!h.records.debug_text().contains("hunter2"));
    // And the same connection can still log in with a key.
    assert_eq!(
        c.login("git", ED_A_PRIVATE, None).await,
        AuthResult::Success
    );
}

#[tokio::test]
async fn rsa_keys_log_in_with_sha2_signatures() {
    let h = start().await;
    h.authz.add_user(RSA2048_PUBLIC, "rsa");
    for hash in [HashAlg::Sha256, HashAlg::Sha512] {
        let mut c = connect(h.addr).await;
        assert_eq!(
            c.login("git", RSA2048_PRIVATE, Some(hash)).await,
            AuthResult::Success,
            "{hash:?}"
        );
    }
    // The server tells the client which RSA signatures to use (`server-sig-algs`): SHA-2 only,
    // best first. An OpenSSH client with an RSA key follows it, so it never signs with SHA-1,
    // even though this host key is ed25519.
    let c = connect(h.addr).await;
    let best = within(10, c.handle.best_supported_rsa_hash())
        .await
        .unwrap();
    assert_eq!(best, Some(Some(HashAlg::Sha512)));
}

/// SPEC §25.11.1 refuses `ssh-rsa` with a SHA-1 signature. `russh` 0.63.3 verifies the
/// signature inside the library and never tells the `Handler` which signature algorithm the
/// client used (it drops `hash_alg` in `server_read_auth_request_pk`), so the listener cannot
/// refuse it yet; it advertises SHA-2 only, which every compliant client follows. This test is
/// the requirement, waiting for a library hook or a patched `russh`.
#[tokio::test]
#[ignore = "russh 0.63.3 does not expose the client's signature algorithm to the Handler"]
async fn rsa_sha1_signature_is_refused() {
    let h = start().await;
    h.authz.add_user(RSA2048_PUBLIC, "rsa");
    let mut c = connect(h.addr).await;
    assert_eq!(c.login("git", RSA2048_PRIVATE, None).await, denied());
}

#[tokio::test]
async fn ecdsa_and_ed25519_keys_log_in() {
    let h = start().await;
    h.authz.add_user(EC_PUBLIC, "carol");
    let mut c = connect(h.addr).await;
    assert_eq!(c.login("git", EC_PRIVATE, None).await, AuthResult::Success);
}

// ------------------------------------------------------------------------------ algorithms

fn packet(payload: &[u8]) -> Vec<u8> {
    let mut padding = 8 - ((payload.len() + 5) % 8);
    if padding < 4 {
        padding += 8;
    }
    let mut p = Vec::new();
    p.extend_from_slice(&((payload.len() + padding + 1) as u32).to_be_bytes());
    p.push(padding as u8);
    p.extend_from_slice(payload);
    p.resize(p.len() + padding, 0);
    p
}

/// An `SSH_MSG_KEXINIT` offering exactly these ten name lists.
fn kexinit(lists: [&str; 10]) -> Vec<u8> {
    let mut payload = vec![20u8];
    payload.extend_from_slice(&[7; 16]);
    for l in lists {
        payload.extend_from_slice(&(l.len() as u32).to_be_bytes());
        payload.extend_from_slice(l.as_bytes());
    }
    payload.push(0);
    payload.extend_from_slice(&0u32.to_be_bytes());
    packet(&payload)
}

async fn read_line(s: &mut TcpStream) -> String {
    let mut line = Vec::new();
    loop {
        let b = within(10, s.read_u8()).await.unwrap();
        line.push(b);
        if b == b'\n' {
            return String::from_utf8_lossy(&line).into_owned();
        }
    }
}

async fn read_packet(s: &mut TcpStream) -> Vec<u8> {
    let len = within(10, s.read_u32()).await.unwrap() as usize;
    let mut pkt = vec![0u8; len];
    within(10, s.read_exact(&mut pkt)).await.unwrap();
    let pad = pkt[0] as usize;
    pkt[1..len - pad].to_vec()
}

/// The server's `KEXINIT` name lists, read off the wire.
async fn server_offer(addr: SocketAddr) -> Vec<Vec<String>> {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"SSH-2.0-probe\r\n").await.unwrap();
    assert!(read_line(&mut s).await.starts_with("SSH-2.0-"));
    let payload = read_packet(&mut s).await;
    assert_eq!(payload[0], 20, "SSH_MSG_KEXINIT");
    let mut rest = &payload[17..];
    let mut lists = Vec::new();
    for _ in 0..10 {
        let n = u32::from_be_bytes(rest[..4].try_into().unwrap()) as usize;
        let names = std::str::from_utf8(&rest[4..4 + n]).unwrap();
        lists.push(
            names
                .split(',')
                .filter(|n| !n.is_empty())
                .map(str::to_owned)
                .collect(),
        );
        rest = &rest[4 + n..];
    }
    lists
}

#[tokio::test]
async fn the_wire_offers_only_the_spec_algorithms() {
    let h = start().await;
    let lists = server_offer(h.addr).await;
    let (kex, hostkey, c2s, s2c, mac_c2s, mac_s2c, comp_c2s, comp_s2c) = (
        &lists[0], &lists[1], &lists[2], &lists[3], &lists[4], &lists[5], &lists[6], &lists[7],
    );
    // Key exchange: curve25519 only, with the strict-kex extension (Terrapin, CVE-2023-48795).
    assert!(
        kex.iter().any(|k| k == "kex-strict-s-v00@openssh.com"),
        "{kex:?}"
    );
    let real: Vec<_> = kex
        .iter()
        .filter(|k| !k.contains("@openssh.com") && !k.starts_with("ext-info"))
        .collect();
    assert_eq!(
        real,
        ["curve25519-sha256", "curve25519-sha256@libssh.org"],
        "{kex:?}"
    );
    assert!(
        kex.iter().all(|k| !k.contains("sha1")),
        "no SHA-1 key exchange: {kex:?}"
    );
    // Ciphers: the three AEADs. No CBC, no CTR, no 3DES.
    let aead = [
        "chacha20-poly1305@openssh.com",
        "aes256-gcm@openssh.com",
        "aes128-gcm@openssh.com",
    ];
    assert_eq!(c2s, &aead);
    assert_eq!(s2c, &aead);
    for m in [mac_c2s, mac_s2c] {
        assert!(
            m.iter().all(|n| !n.contains("sha1") && !n.contains("md5")),
            "{m:?}"
        );
    }
    // No compression.
    assert_eq!(comp_c2s, &["none"]);
    assert_eq!(comp_s2c, &["none"]);
    // The host key: ed25519 only (no RSA host key is configured), never `ssh-rsa`.
    assert_eq!(hostkey, &["ssh-ed25519"]);
}

/// Sends a client `KEXINIT` and reports whether the server went on to key exchange or closed.
async fn negotiate(addr: SocketAddr, offer: [&str; 10]) -> bool {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"SSH-2.0-probe\r\n").await.unwrap();
    read_line(&mut s).await;
    let _server_kexinit = read_packet(&mut s).await;
    s.write_all(&kexinit(offer)).await.unwrap();
    // Key exchange proceeds only if the server answers once we send a plausible first message
    // (`SSH_MSG_KEX_ECDH_INIT`); a refused negotiation closes the socket instead.
    let mut init = vec![30u8];
    init.extend_from_slice(&32u32.to_be_bytes());
    init.extend_from_slice(&[9; 32]);
    let _ = s.write_all(&packet(&init)).await;
    let mut got = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut got)).await;
    // A reply (SSH_MSG_KEX_ECDH_REPLY, 31) in the bytes means the server went on.
    got.windows(1).any(|w| w[0] == 31) && got.len() > 200
}

#[tokio::test]
async fn weak_algorithms_cannot_be_negotiated() {
    let h = start().await;
    let good = [
        "curve25519-sha256",
        "ssh-ed25519",
        "chacha20-poly1305@openssh.com",
        "chacha20-poly1305@openssh.com",
        "hmac-sha2-256",
        "hmac-sha2-256",
        "none",
        "none",
        "",
        "",
    ];
    let with = |i: usize, v: &'static str| {
        let mut o = good;
        o[i] = v;
        o
    };
    // SHA-1 key exchange, the CBC and CTR ciphers, SHA-1 MACs alone, and compression.
    assert!(!negotiate(h.addr, with(0, "diffie-hellman-group14-sha1")).await);
    assert!(!negotiate(h.addr, with(0, "diffie-hellman-group1-sha1")).await);
    for cipher in ["aes128-cbc", "aes256-cbc", "3des-cbc", "aes256-ctr"] {
        // The MAC is `none`, the one the server offers, so only the cipher can fail the match.
        let mut o = with(2, cipher);
        o[3] = cipher;
        o[4] = "none";
        o[5] = "none";
        assert!(!negotiate(h.addr, o).await, "{cipher}");
    }
    let mut zlib = good;
    zlib[6] = "zlib@openssh.com";
    zlib[7] = "zlib@openssh.com";
    assert!(!negotiate(h.addr, zlib).await);
    // A host key algorithm the server does not have.
    assert!(!negotiate(h.addr, with(1, "ssh-rsa")).await);
}

// ------------------------------------------------------------------------------ channels

#[tokio::test]
async fn shell_greets_and_exits_one_and_is_audited() {
    let h = start().await;
    let mut c = alice(&h).await;
    let mut ch = c.session().await.unwrap();
    ch.request_shell(true).await.unwrap();
    let r = drain(&mut ch).await;
    assert_eq!(r.exit, Some(1));
    assert!(
        r.stderr.starts_with(
            "Hi pat:alice! You've successfully authenticated, but asc does not provide shell access."
        ),
        "{r:?}"
    );
    assert!(r.closed);
    eventually("audit", || h.records.refused_names() == ["shell"]).await;
    assert_eq!(h.metrics.refused_total("shell"), 1);
    assert_eq!(h.exec.started.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn the_command_table() {
    let h = start().await;
    let mut c = alice(&h).await;
    let long = format!("git-upload-pack '{}'", "a/".repeat(130));
    let refused: Vec<(Vec<u8>, &str)> = vec![
        (b"git-upload-archive 'o/r'".to_vec(), "archive"),
        (b"git-lfs-authenticate 'o/r' download".to_vec(), "lfs"),
        (b"git upload-pack 'o/r'".to_vec(), "git space"),
        (b"scp -t /tmp".to_vec(), "scp"),
        (b"sftp".to_vec(), "sftp"),
        (b"asc-authenticate".to_vec(), "asc-authenticate"),
        (b"sh -c id".to_vec(), "shell line"),
        (b"git-upload-pack '../x'".to_vec(), "dotdot"),
        (b"git-upload-pack 'o/r;id'".to_vec(), "semicolon"),
        (b"git-upload-pack '$(id)'".to_vec(), "subst"),
        (b"git-upload-pack 'o/`id`'".to_vec(), "backtick"),
        (b"git-upload-pack 'a'\\''b'".to_vec(), "escaped quote"),
        (long.into_bytes(), "257 bytes"),
        (b"git-upload-pack 'o/r\0'".to_vec(), "NUL"),
        (b"git-upload-pack 'o/\x01r'".to_vec(), "control"),
        (b"git-upload-pack o/r".to_vec(), "unquoted"),
    ];
    for (command, why) in refused {
        let r = run(&mut c, &command).await;
        assert_eq!(r.stderr, "asc: command not allowed\n", "{why}");
        assert_eq!(r.exit, Some(1), "{why}");
        assert!(r.stdout.is_empty(), "{why}");
    }
    assert_eq!(
        h.exec.started.load(Ordering::SeqCst),
        0,
        "no refused command reached the exec"
    );
    assert_eq!(
        h.records
            .refused_names()
            .iter()
            .filter(|n| **n == "exec")
            .count(),
        16
    );

    // The two good forms, with and without the leading slash and the suffix.
    *h.exec.mode.lock().unwrap() = Mode::Reply(b"hello", 0);
    for (command, verb, repo) in [
        ("git-upload-pack 'o/r.git'", "git-upload-pack", "o/r"),
        ("git-upload-pack '/o/r'", "git-upload-pack", "o/r"),
        (
            "git-receive-pack 'owner/my-repo'",
            "git-receive-pack",
            "owner/my-repo",
        ),
    ] {
        let r = run(&mut c, command.as_bytes()).await;
        assert_eq!(r.stdout, b"hello", "{command}");
        assert_eq!(r.exit, Some(0), "{command}");
        let seen = h.exec.seen.lock().unwrap().last().unwrap().clone();
        assert_eq!((seen.verb, seen.repo.as_str()), (verb, repo), "{command}");
        assert_eq!(seen.principal, "pat:alice");
        assert_eq!(seen.fingerprint, fingerprint(ED_A_PUBLIC));
        assert_eq!(seen.key_kind, "user");
    }
    // The exec asked the authorizer itself, once per command.
    assert_eq!(h.authz.authorize_calls.load(Ordering::SeqCst), 3);
    eventually("command records", || h.records.commands().len() == 3).await;
    let rec = &h.records.commands()[0];
    assert_eq!(
        (rec.verb, rec.repo.as_str(), rec.outcome.as_str()),
        ("git-upload-pack", "o/r", "ok")
    );
}

#[tokio::test]
async fn the_exit_status_and_output_of_the_command_reach_the_client() {
    let h = start().await;
    let mut c = alice(&h).await;
    *h.exec.mode.lock().unwrap() = Mode::Reply(b"out", 7);
    let r = run(&mut c, b"git-receive-pack 'o/r'").await;
    assert_eq!((r.stdout.as_slice(), r.exit), (&b"out"[..], Some(7)));
    eventually("record", || {
        h.records
            .commands()
            .first()
            .is_some_and(|c| c.outcome == "exit:7")
    })
    .await;
}

#[tokio::test]
async fn client_input_reaches_the_command_until_eof() {
    let h = start().await;
    let mut c = alice(&h).await;
    *h.exec.mode.lock().unwrap() = Mode::Echo;
    let mut ch = c.session().await.unwrap();
    ch.exec(true, b"git-receive-pack 'o/r'".to_vec())
        .await
        .unwrap();
    ch.data_bytes(vec![b'x'; 100_000]).await.unwrap();
    ch.eof().await.unwrap();
    let r = drain(&mut ch).await;
    assert_eq!(r.stdout.len(), 100_000);
    assert_eq!(r.exit, Some(0));
}

#[tokio::test]
async fn git_protocol_is_the_only_environment_variable() {
    let h = start().await;
    let mut c = alice(&h).await;
    let mut ch = c.session().await.unwrap();
    for (name, value, ok) in [
        ("LD_PRELOAD", "/tmp/x.so", false),
        ("GIT_SSH_COMMAND", "id", false),
        ("GIT_PROTOCOL", "version=9", false),
        ("GIT_PROTOCOL", "version=2;id", false),
        ("LANG", "version=2", false),
        ("GIT_PROTOCOL", "version=2", true),
    ] {
        ch.set_env(true, name, value).await.unwrap();
        assert_eq!(first_reply(&mut ch).await, Some(ok), "{name}={value}");
    }
    ch.exec(true, b"git-upload-pack 'o/r'".to_vec())
        .await
        .unwrap();
    let r = drain(&mut ch).await;
    assert_eq!(r.exit, Some(0));
    let seen = h.exec.seen.lock().unwrap()[0].clone();
    assert_eq!(seen.git_protocol.as_deref(), Some("version=2"));
    assert_eq!(
        h.records
            .refused_names()
            .iter()
            .filter(|n| **n == "env")
            .count(),
        5
    );
    // Without an env request the command sees none.
    let _ = run(&mut c, b"git-upload-pack 'o/r'").await;
    assert_eq!(h.exec.seen.lock().unwrap()[1].git_protocol, None);
}

#[tokio::test]
async fn every_row_of_the_request_table_is_refused_and_audited() {
    let h = start().await;
    let mut c = alice(&h).await;
    let mut ch = c.session().await.unwrap();
    ch.request_subsystem(true, "sftp").await.unwrap();
    assert_eq!(first_reply(&mut ch).await, Some(false), "subsystem");
    ch.request_pty(true, "xterm", 80, 24, 0, 0, &[])
        .await
        .unwrap();
    assert_eq!(first_reply(&mut ch).await, Some(false), "pty-req");
    ch.request_x11(true, false, "MIT-MAGIC-COOKIE-1", "00", 0)
        .await
        .unwrap();
    assert_eq!(first_reply(&mut ch).await, Some(false), "x11-req");
    ch.set_env(true, "TERM", "xterm").await.unwrap();
    assert_eq!(first_reply(&mut ch).await, Some(false), "env");
    // russh answers this one with a global request failure the channel never sees, so only the
    // audit record below proves it was refused.
    ch.agent_forward(false).await.unwrap();

    // A second exec on a channel is refused.
    *h.exec.mode.lock().unwrap() = Mode::Hang;
    ch.exec(true, b"git-upload-pack 'o/r'".to_vec())
        .await
        .unwrap();
    assert_eq!(first_reply(&mut ch).await, Some(true));
    ch.exec(true, b"git-upload-pack 'o/r'".to_vec())
        .await
        .unwrap();
    assert_eq!(first_reply(&mut ch).await, Some(false), "second exec");

    // Channel opens and global requests.
    let prohibited = |e: russh::Error| {
        matches!(
            e,
            russh::Error::ChannelOpenFailure(ChannelOpenFailure::AdministrativelyProhibited)
        )
    };
    let e = within(
        10,
        c.handle
            .channel_open_direct_tcpip("127.0.0.1", 22, "127.0.0.1", 4000),
    )
    .await
    .unwrap_err();
    assert!(prohibited(e), "direct-tcpip");
    let e = within(
        10,
        c.handle.channel_open_direct_streamlocal("/var/run/x.sock"),
    )
    .await
    .unwrap_err();
    assert!(prohibited(e), "direct-streamlocal");
    assert!(
        within(10, c.handle.tcpip_forward("127.0.0.1", 0))
            .await
            .is_err()
    );
    assert!(
        within(10, c.handle.streamlocal_forward("/tmp/x.sock"))
            .await
            .is_err()
    );

    let want = [
        "subsystem",
        "pty-req",
        "x11-req",
        "env",
        "auth-agent-req@openssh.com",
        "exec",
        "direct-tcpip",
        "direct-streamlocal@openssh.com",
        "tcpip-forward",
        "streamlocal-forward@openssh.com",
    ];
    eventually("all refusals audited", || {
        let names = h.records.refused_names();
        want.iter().all(|w| names.contains(w))
    })
    .await;
    for name in want {
        assert!(h.metrics.refused_total(name) >= 1, "{name}");
    }
    // Every record names the key that made the request.
    for r in h.records.refused() {
        assert_eq!(r.fingerprint, fingerprint(ED_A_PUBLIC));
        assert_eq!(r.principal, "pat:alice");
    }
    // Only the one valid exec reached the command runner.
    assert_eq!(h.exec.started.load(Ordering::SeqCst), 1);
    // The connection still works.
    *h.exec.mode.lock().unwrap() = Mode::Reply(b"ok", 0);
    assert_eq!(run(&mut c, b"git-upload-pack 'o/r'").await.exit, Some(0));
}

#[tokio::test]
async fn a_connection_with_too_many_refused_requests_is_dropped() {
    let h = start().await;
    let mut c = alice(&h).await;
    let ch = c.session().await.unwrap();
    for _ in 0..200 {
        if ch.set_env(false, "LD_PRELOAD", "x").await.is_err() {
            break;
        }
    }
    within(10, async {
        while !c.handle.is_closed() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    assert!(
        h.records.refused().len() <= 64,
        "{}",
        h.records.refused().len()
    );
}

// ------------------------------------------------------------------------------ limits

#[tokio::test]
async fn session_channels_and_running_commands_are_capped() {
    let h = start_with(
        |_, l| {
            l.max_channels = 3;
            l.max_running_commands = 1;
        },
        Limits::default(),
    )
    .await;
    let mut c = alice(&h).await;
    *h.exec.mode.lock().unwrap() = Mode::Hang;
    let mut first = c.session().await.unwrap();
    first
        .exec(true, b"git-upload-pack 'o/r'".to_vec())
        .await
        .unwrap();
    assert_eq!(first_reply(&mut first).await, Some(true));
    eventually("command started", || {
        h.exec.started.load(Ordering::SeqCst) == 1
    })
    .await;

    // A second command while one runs: told to retry, nothing started.
    let mut second = c.session().await.unwrap();
    second
        .exec(true, b"git-upload-pack 'o/r'".to_vec())
        .await
        .unwrap();
    let r = drain(&mut second).await;
    assert_eq!(
        r.stderr,
        "asc: too many concurrent operations, retry later\n"
    );
    assert_eq!(r.exit, Some(1));
    assert_eq!(h.exec.started.load(Ordering::SeqCst), 1);

    // Channels: `first` is open and running; two more fill the cap of three; the next is
    // refused and audited.
    let extra = c.session().await.unwrap();
    let _spare = c.session().await.unwrap();
    let e = c.session().await.unwrap_err();
    assert!(
        matches!(
            e,
            russh::Error::ChannelOpenFailure(ChannelOpenFailure::ResourceShortage)
        ),
        "{e:?}"
    );
    eventually("audited", || h.records.refused_names().contains(&"session")).await;
    // Closing one frees its slot.
    extra.close().await.unwrap();
    let mut freed = false;
    for _ in 0..100 {
        if c.session().await.is_ok() {
            freed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(freed, "a closed channel frees its slot");
}

#[tokio::test]
async fn a_command_past_the_request_timeout_is_stopped() {
    let h = start_with(
        |_, l| {
            l.request_timeout = Secs(1);
            l.idle_timeout = Secs(1);
        },
        Limits::default(),
    )
    .await;
    let mut c = alice(&h).await;
    *h.exec.mode.lock().unwrap() = Mode::Hang;
    let r = run(&mut c, b"git-receive-pack 'o/r'").await;
    assert_eq!(r.exit, Some(1));
    assert!(r.stderr.contains("took too long"), "{r:?}");
    assert_eq!(
        h.exec.dropped.load(Ordering::SeqCst),
        1,
        "the command was dropped (killed)"
    );
    eventually("record", || {
        h.records
            .commands()
            .first()
            .is_some_and(|c| c.outcome == "limit:request_timeout")
    })
    .await;
}

#[tokio::test]
async fn closing_the_connection_stops_its_commands() {
    let h = start().await;
    let mut c = alice(&h).await;
    *h.exec.mode.lock().unwrap() = Mode::Hang;
    let mut ch = c.session().await.unwrap();
    ch.exec(true, b"git-upload-pack 'o/r'".to_vec())
        .await
        .unwrap();
    assert_eq!(first_reply(&mut ch).await, Some(true));
    eventually("started", || h.exec.started.load(Ordering::SeqCst) == 1).await;
    drop(ch);
    within(
        5,
        c.handle
            .disconnect(russh::Disconnect::ByApplication, "bye", "en"),
    )
    .await
    .unwrap();
    eventually("killed", || h.exec.dropped.load(Ordering::SeqCst) == 1).await;
}

#[tokio::test]
async fn max_auth_tries_disconnects() {
    let h = start_with(|_, l| l.max_auth_tries = 2, Limits::default()).await;
    let mut c = connect(h.addr).await;
    let key = || russh::keys::PrivateKeyWithHashAlg::new(private(ED_C_PRIVATE), None);
    for _ in 0..2 {
        let r = within(10, c.handle.authenticate_publickey("git", key()))
            .await
            .unwrap();
        assert_eq!(r, denied());
    }
    // The next attempt is not answered: the server disconnects.
    let third = within(10, c.handle.authenticate_publickey("git", key())).await;
    assert!(third.is_err() || third.unwrap() != AuthResult::Success);
    within(10, async {
        while !c.handle.is_closed() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
}

#[tokio::test]
async fn auth_timeout_closes_a_connection_that_never_speaks() {
    let h = start_with(|_, l| l.auth_timeout = Secs(1), Limits::default()).await;
    // A raw socket that sends nothing.
    let mut s = TcpStream::connect(h.addr).await.unwrap();
    let mut got = Vec::new();
    within(10, s.read_to_end(&mut got)).await.unwrap();
    assert!(
        got.starts_with(b"SSH-2.0-asc"),
        "{:?}",
        String::from_utf8_lossy(&got)
    );
    eventually("auth_timeout record", || {
        h.records
            .outcomes()
            .contains(&"limit:auth_timeout".to_owned())
    })
    .await;
}

#[tokio::test]
async fn auth_timeout_closes_a_connection_that_never_logs_in() {
    let h = start_with(|_, l| l.auth_timeout = Secs(2), Limits::default()).await;
    // Key exchange completes; no login follows.
    let c = connect(h.addr).await;
    within(10, async {
        while !c.handle.is_closed() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    eventually("auth_timeout record", || {
        h.records
            .outcomes()
            .contains(&"limit:auth_timeout".to_owned())
    })
    .await;
    assert_eq!(h.metrics.logins_total("limit:auth_timeout"), 1);
}

/// What a refused connection receives: the identification string and `SSH_MSG_DISCONNECT`.
async fn is_turned_away(addr: SocketAddr) -> bool {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut got = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut got)).await;
    let text = String::from_utf8_lossy(&got);
    got.starts_with(b"SSH-2.0-asc") && text.contains("too many connections")
}

#[tokio::test]
async fn unauthenticated_connections_are_capped_per_peer() {
    let h = start_with(
        |_, l| {
            l.max_unauthenticated = 4;
            l.max_unauthenticated_per_peer = 2;
        },
        Limits::default(),
    )
    .await;
    // Two connections that say nothing hold the peer's two slots.
    let _a = TcpStream::connect(h.addr).await.unwrap();
    let _b = TcpStream::connect(h.addr).await.unwrap();
    eventually("both admitted", || {
        h.metrics.connections_total("accepted") == 2
    })
    .await;
    assert!(is_turned_away(h.addr).await);
    eventually("record", || {
        h.records
            .outcomes()
            .contains(&"limit:unauthenticated_per_peer".to_owned())
    })
    .await;
    assert_eq!(
        h.metrics
            .connections_total("limit:unauthenticated_per_peer"),
        1
    );
}

#[tokio::test]
async fn unauthenticated_connections_are_capped_in_total() {
    let h = start_with(
        |_, l| {
            l.max_unauthenticated = 2;
            l.max_unauthenticated_per_peer = 2;
        },
        Limits::default(),
    )
    .await;
    let _a = TcpStream::connect(h.addr).await.unwrap();
    let _b = TcpStream::connect(h.addr).await.unwrap();
    eventually("both admitted", || {
        h.metrics.connections_total("accepted") == 2
    })
    .await;
    assert!(is_turned_away(h.addr).await);
    assert_eq!(h.metrics.connections_total("limit:unauthenticated"), 1);
}

#[tokio::test]
async fn logging_in_releases_the_unauthenticated_slot() {
    let h = start_with(
        |_, l| {
            l.max_unauthenticated = 4;
            l.max_unauthenticated_per_peer = 1;
        },
        Limits::default(),
    )
    .await;
    let _first = alice(&h).await;
    // The peer's one unauthenticated slot is free again: a second client gets all the way in.
    let mut second = connect(h.addr).await;
    assert_eq!(
        second.login("git", ED_A_PRIVATE, None).await,
        AuthResult::Success
    );
}

#[tokio::test]
async fn connections_are_capped_per_replica() {
    let h = start_with(
        |_, l| {
            l.max_connections = 2;
            l.max_unauthenticated = 2;
            l.max_unauthenticated_per_peer = 2;
        },
        Limits::default(),
    )
    .await;
    let _a = alice(&h).await;
    let _b = connect(h.addr).await;
    assert!(is_turned_away(h.addr).await);
    assert_eq!(h.metrics.connections_total("limit:connections"), 1);
}

#[tokio::test]
async fn new_connections_are_rate_limited_per_peer() {
    let h = start_with(
        |_, l| {
            l.new_connections_per_sec = 1;
            l.new_connections_burst = 3;
        },
        Limits::default(),
    )
    .await;
    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(TcpStream::connect(h.addr).await.unwrap());
    }
    assert!(is_turned_away(h.addr).await);
    eventually("record", || {
        h.records
            .outcomes()
            .contains(&"limit:connection_rate".to_owned())
    })
    .await;
}

#[tokio::test]
async fn a_principal_has_a_connection_cap() {
    let h = start_with(
        |_, l| l.max_connections_per_principal = 1,
        Limits::default(),
    )
    .await;
    let _a = alice(&h).await;
    let mut b = connect(h.addr).await;
    assert_eq!(b.login("git", ED_A_PRIVATE, None).await, denied());
    eventually("record", || {
        h.records
            .outcomes()
            .contains(&"limit:principal_connections".to_owned())
    })
    .await;
    // Another principal is not affected.
    h.authz.add_user(ED_B_PUBLIC, "bob");
    let mut other = connect(h.addr).await;
    assert_eq!(
        other.login("git", ED_B_PRIVATE, None).await,
        AuthResult::Success
    );
}

#[tokio::test]
async fn failed_logins_feed_the_counters_http_uses() {
    let shared = Limits {
        cheap_burst: 30,
        cheap_rps: 1,
        ..Limits::default()
    };
    let h = start_with(|_, _| {}, shared).await;
    let peer = Caller::Unverified(Peer::from_ip(Some(LOCAL.into())));
    // An HTTP-style request from that address still passes.
    assert!(h.admission.admit(peer, Class::Cheap, 1).is_ok());
    // Four connections cost 4 of the 30; only the charge of each failed login (unknown key) can
    // spend the rest. One probe, not a polling loop: a probe that succeeds spends a token too.
    for _ in 0..4 {
        let mut c = connect(h.addr).await;
        assert_eq!(c.login("git", ED_C_PRIVATE, None).await, denied());
    }
    assert!(h.admission.admit(peer, Class::Cheap, 1).is_err());
    // And SSH admission turns the peer away, as HTTP admission would.
    assert!(is_turned_away(h.addr).await);
    assert!(h.metrics.connections_total("limit:admission") >= 1);
}

#[tokio::test]
async fn a_silent_client_is_closed_by_keepalives() {
    let h = start_with(
        |_, l| {
            l.keepalive = Secs(1);
            l.keepalive_max = 1;
        },
        Limits::default(),
    )
    .await;
    h.authz.add_user(ED_A_PUBLIC, "alice");
    // A relay that can stop passing the client's bytes: the client lives, but it goes silent.
    let relay = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_addr = relay.local_addr().unwrap();
    let mute = Arc::new(AtomicBool::new(false));
    let upstream = h.addr;
    let muted = mute.clone();
    tokio::spawn(async move {
        let (client, _) = relay.accept().await.unwrap();
        let server = TcpStream::connect(upstream).await.unwrap();
        let (mut cr, mut cw) = client.into_split();
        let (mut sr, mut sw) = server.into_split();
        let up = async move {
            let mut buf = [0u8; 4096];
            while let Ok(n) = cr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                if !muted.load(Ordering::SeqCst) && sw.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        };
        let down = async move {
            let _ = tokio::io::copy(&mut sr, &mut cw).await;
        };
        tokio::join!(up, down);
    });
    let mut c = connect(relay_addr).await;
    assert_eq!(
        c.login("git", ED_A_PRIVATE, None).await,
        AuthResult::Success
    );
    eventually("session open", || h.metrics.sessions() == 1).await;
    mute.store(true, Ordering::SeqCst);
    // The server's keep-alives get no answer: it closes the connection.
    eventually("session closed by keepalive", || h.metrics.sessions() == 0).await;
}

// ------------------------------------------------------------------------------ host key

#[tokio::test]
async fn the_host_key_is_created_once_and_kept_across_a_restart() {
    let h = start().await;
    let before = h.host_fingerprint;
    let c = connect(h.addr).await;
    assert_eq!(c.host.lock().unwrap().unwrap(), before);
    drop(c);
    let key_file = h.host_key_file.clone();
    let on_disk = std::fs::read_to_string(&key_file).unwrap();
    assert!(on_disk.contains("OPENSSH PRIVATE KEY"));
    // Restart: a new listener over the same directory presents the same key.
    let dir = h.stop();
    let again = start_in(dir, |_, _| {}, Limits::default()).await;
    assert_eq!(again.host_fingerprint, before);
    let c = connect(again.addr).await;
    assert_eq!(c.host.lock().unwrap().unwrap(), before);
    assert_eq!(std::fs::read_to_string(&key_file).unwrap(), on_disk);
}

#[cfg(unix)]
#[tokio::test]
async fn a_group_readable_host_key_stops_the_listener() {
    use std::os::unix::fs::PermissionsExt as _;
    let h = start().await;
    let host_key_file = h.host_key_file.clone();
    let dir = h.stop();
    std::fs::set_permissions(&host_key_file, std::fs::Permissions::from_mode(0o640)).unwrap();
    let ssh = asc_serve::config::SshConfig {
        enabled: true,
        listen: "127.0.0.1:0".parse().unwrap(),
        host_key_files: vec![host_key_file],
        ..Default::default()
    };
    let result = asc_serve::ssh::SshServer::bind(
        &ssh,
        &test_limits(),
        asc_serve::ssh::SshDeps {
            authorizer: Arc::new(FakeAuthz::default()),
            admission: None,
            audit: asc_serve::ssh::Audit::new(
                Arc::new(Records::default()),
                asc_serve::ssh::SshMetrics::detached(),
            ),
            exec: Arc::new(asc_serve::ssh::Unavailable),
        },
    )
    .await;
    assert!(result.is_err());
    drop(dir);
}

// ------------------------------------------------------------------------------ start

#[tokio::test]
async fn the_default_exec_says_git_over_ssh_is_not_available() {
    let dir = tempfile::tempdir().unwrap();
    let authz = Arc::new(FakeAuthz::default());
    authz.add_user(ED_A_PUBLIC, "alice");
    let ssh = asc_serve::config::SshConfig {
        enabled: true,
        listen: "127.0.0.1:0".parse().unwrap(),
        host_key_files: vec![dir.path().join("hk")],
        ..Default::default()
    };
    let server = asc_serve::ssh::SshServer::bind(
        &ssh,
        &test_limits(),
        asc_serve::ssh::SshDeps {
            authorizer: authz,
            admission: None,
            audit: asc_serve::ssh::Audit::new(
                Arc::new(Records::default()),
                asc_serve::ssh::SshMetrics::detached(),
            ),
            exec: Arc::new(asc_serve::ssh::Unavailable),
        },
    )
    .await
    .unwrap();
    let addr = server.local_addr().unwrap();
    let task = tokio::spawn(server.run(std::future::pending()));
    let mut c = connect(addr).await;
    assert_eq!(
        c.login("git", ED_A_PRIVATE, None).await,
        AuthResult::Success
    );
    let r = run(&mut c, b"git-upload-pack 'o/r'").await;
    assert_eq!(r.stderr, "asc: git over ssh is not available\n");
    assert_eq!(r.exit, Some(1));
    task.abort();
}

fn config(extra: &str) -> asc_serve::ServeConfig {
    asc_serve::ServeConfig::parse(&format!(
        "store = \"memory:///\"\ndb = \"sqlite::memory:\"\n{extra}"
    ))
    .unwrap()
}

fn deps(authz: Arc<FakeAuthz>) -> asc_serve::ssh::StartDeps {
    asc_serve::ssh::StartDeps {
        authorizer: authz,
        admission: Arc::new(asc_serve::limits::Admission::new(&Limits::default())),
        metrics: Arc::new(asc_serve::http::Metrics::new()),
        exec: Arc::new(asc_serve::ssh::Unavailable),
    }
}

#[tokio::test]
async fn the_listener_is_not_bound_unless_every_condition_holds() {
    let dir = tempfile::tempdir().unwrap();
    let hk = dir
        .path()
        .join("hk")
        .display()
        .to_string()
        .replace('\\', "/");
    let ssh =
        format!("[ssh]\nenabled = true\nlisten = \"127.0.0.1:0\"\nhost_key_files = [\"{hk}\"]\n");
    let store = "[auth]\nprovider = \"token_store\"\ntoken_key_file = \"k\"\n";

    // All conditions: bound.
    let ok = config(&format!("{store}{ssh}"));
    let server = asc_serve::ssh::start(&ok, true, deps(Arc::default()))
        .await
        .unwrap();
    assert!(server.is_some());
    drop(server);

    // `[git] enabled = false`: not bound, and the host key is not even created.
    let dir2 = tempfile::tempdir().unwrap();
    let hk2 = dir2
        .path()
        .join("hk")
        .display()
        .to_string()
        .replace('\\', "/");
    let ssh2 =
        format!("[ssh]\nenabled = true\nlisten = \"127.0.0.1:0\"\nhost_key_files = [\"{hk2}\"]\n");
    let no_git = config(&format!("{store}[git]\nenabled = false\n{ssh2}"));
    assert!(
        asc_serve::ssh::start(&no_git, true, deps(Arc::default()))
            .await
            .unwrap()
            .is_none()
    );
    assert!(!dir2.path().join("hk").exists());

    // `provider = "forgejo"` on the public API alone (Stage 1): not bound.
    #[cfg(feature = "forgejo")]
    {
        let forge = config(&format!(
            "[auth]\nprovider = \"forgejo\"\n[forgejo]\nurl = \"https://forge.example\"\n{ssh2}"
        ));
        assert!(
            asc_serve::ssh::start(&forge, true, deps(Arc::default()))
                .await
                .unwrap()
                .is_none()
        );
        assert!(!dir2.path().join("hk").exists());
    }

    // Off by default.
    let off = config(store);
    assert!(
        asc_serve::ssh::start(&off, true, deps(Arc::default()))
            .await
            .unwrap()
            .is_none()
    );
}

#[test]
fn the_listener_pulls_in_no_ssh_crate_without_the_feature() {
    // The build gate runs `cargo tree -p asc-serve --no-default-features`; here the source is
    // checked: nothing outside `ssh/` and `authz/ssh_keys.rs` names the library.
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel.starts_with("ssh/") || rel == "authz/ssh_keys.rs" || rel == "authz/key_file.rs" {
                continue;
            }
            if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                let code = text.lines().filter(|l| !l.trim_start().starts_with("//"));
                for line in code {
                    assert!(!line.contains("russh::"), "{rel}: {line}");
                }
            }
        }
    }
}
