//! `asc auth login` and `asc auth logout`: the token goes to the git credential helpers through
//! `git credential approve` and `reject`, read from stdin, never argv, never printed.
#![allow(clippy::unwrap_used)]

mod support;

use std::io::Write;
use std::path::Path;
use std::process::{Output, Stdio};

use support::{Sandbox, text};

const TOKEN: &str = "canary-S3CR3T-token-do-not-leak";

fn sh_path(p: &Path) -> String {
    p.display().to_string().replace(char::from(92), "/")
}

/// Installs a logging credential helper in the sandbox's git config; returns its log path.
fn install_helper(sb: &Sandbox) -> std::path::PathBuf {
    let script = sb.tmp.path().join("helper.sh");
    let log = sb.tmp.path().join("helper.log");
    let body = format!(
        "#!/bin/sh\n{{ echo \"== $1\"; cat; echo '--'; }} >> '{}'\n",
        sh_path(&log)
    );
    std::fs::write(&script, body).unwrap();
    std::fs::write(
        sb.user.join(".gitconfig"),
        format!("[credential]\n\thelper = !sh '{}'\n", sh_path(&script)),
    )
    .unwrap();
    log
}

fn calls(log: &Path) -> String {
    std::fs::read_to_string(log).unwrap_or_default()
}

fn asc(sb: &Sandbox, args: &[&str], stdin: Option<&str>, loopback: bool) -> Output {
    let cwd = sb.dir("elsewhere");
    let mut cmd = sb.asc(&cwd);
    cmd.env("ASC_NO_DAEMON", "1").arg("auth").args(args);
    if !loopback {
        cmd.env_remove("ASC_ALLOW_LOOPBACK_HTTP");
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn asc");
    let mut pipe = child.stdin.take().unwrap();
    if let Some(s) = stdin {
        pipe.write_all(s.as_bytes()).unwrap();
    }
    drop(pipe);
    child.wait_with_output().unwrap()
}

fn never_leaks(out: &Output) {
    assert!(!text(&out.stdout).contains(TOKEN));
    assert!(!text(&out.stderr).contains(TOKEN));
}

#[test]
fn login_approves_the_token_from_stdin() {
    let sb = Sandbox::new();
    let log = install_helper(&sb);
    let out = asc(
        &sb,
        &[
            "login",
            "--host",
            "https://hub.example.com:8443",
            "--token-stdin",
        ],
        Some(&format!("{TOKEN}\n")),
        false,
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    never_leaks(&out);
    let log = calls(&log);
    assert!(log.contains("== store"), "{log}");
    assert!(log.contains("protocol=https\n"), "{log}");
    assert!(log.contains("host=hub.example.com:8443\n"), "{log}");
    assert!(log.contains("username=asc\n"), "{log}");
    assert!(log.contains(&format!("password={TOKEN}\n")), "{log}");
}

#[test]
fn logout_rejects_the_stored_credential() {
    let sb = Sandbox::new();
    let log = install_helper(&sb);
    let out = asc(
        &sb,
        &["logout", "--host", "https://hub.example.com"],
        None,
        false,
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    let log = calls(&log);
    assert!(log.contains("== erase"), "{log}");
    assert!(log.contains("host=hub.example.com\n"), "{log}");
    assert!(!log.contains("password="), "{log}");
}

#[test]
fn plain_http_is_refused_except_loopback_with_opt_in() {
    let sb = Sandbox::new();
    let log = install_helper(&sb);
    let login = |host: &str, loopback: bool| {
        asc(
            &sb,
            &["login", "--host", host, "--token-stdin"],
            Some(TOKEN),
            loopback,
        )
    };
    for (host, loopback) in [
        ("http://hub.example.com", false),
        ("http://hub.example.com", true),
        ("http://127.0.0.1:9", false),
    ] {
        let out = login(host, loopback);
        assert!(!out.status.success(), "{host} loopback={loopback}");
        never_leaks(&out);
    }
    assert_eq!(calls(&log), "");
    let out = login("http://127.0.0.1:9", true);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(calls(&log).contains("host=127.0.0.1:9\n"));
}

#[test]
fn bad_input_stores_nothing() {
    let sb = Sandbox::new();
    let log = install_helper(&sb);
    for (args, stdin) in [
        // No host: nothing to sign in to (without a method the browser flow would start).
        (vec!["login", "--token-stdin"], Some(TOKEN)),
        (vec!["login"], None),
        (
            vec!["login", "--host", "https://h.example.com", "--token-stdin"],
            Some("\n"),
        ),
        (
            vec!["login", "--host", "https://h.example.com", "--token-stdin"],
            Some("a\nb\n"),
        ),
        (
            vec![
                "login",
                "--host",
                "https://u:p@h.example.com",
                "--token-stdin",
            ],
            Some(TOKEN),
        ),
        (
            vec!["login", "--host", "ssh://h.example.com", "--token-stdin"],
            Some(TOKEN),
        ),
        // stdin is a pipe here, not a terminal.
        (
            vec!["login", "--host", "https://h.example.com", "--prompt"],
            None,
        ),
        (vec!["logout"], None),
    ] {
        let out = asc(&sb, &args, stdin, false);
        assert!(!out.status.success(), "{args:?}");
        never_leaks(&out);
    }
    assert_eq!(calls(&log), "");
}

/// A loopback hub that answers the device flow: the code, one "pending", then the token.
fn device_hub(token: &'static str) -> (String, std::thread::JoinHandle<Vec<String>>) {
    use std::io::Read;
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let b2 = base.clone();
    let h = std::thread::spawn(move || {
        let mut paths = Vec::new();
        let script = [
            (200, format!("{{\"device_code\":\"dc9\",\"user_code\":\"WXYZ-1234\",\"verification_uri\":\"{b2}/login/device\",\"verification_uri_complete\":\"{b2}/login/device?user_code=WXYZ-1234\",\"expires_in\":600,\"interval\":1}}")),
            (400, "{\"error\":\"authorization_pending\"}".to_owned()),
            (200, format!("{{\"access_token\":\"{token}\",\"token_type\":\"bearer\"}}")),
        ];
        for (status, body) in script {
            let (mut c, _) = l.accept().unwrap();
            let mut buf = vec![0u8; 8192];
            let mut n = 0;
            while !String::from_utf8_lossy(&buf[..n]).contains("\r\n\r\n") {
                let r = c.read(&mut buf[n..]).unwrap();
                if r == 0 {
                    break;
                }
                n += r;
            }
            let head = String::from_utf8_lossy(&buf[..n]).into_owned();
            // Read the body too: closing with unread data would reset the connection.
            let h = head.find("\r\n\r\n").unwrap_or(0) + 4;
            let want: usize = head
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                .unwrap_or(0);
            while n < h + want {
                let r = c.read(&mut buf[n..]).unwrap();
                if r == 0 {
                    break;
                }
                n += r;
            }
            paths.push(head.split_whitespace().nth(1).unwrap_or("").to_owned());
            let resp = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
            let _ = c.write_all(resp.as_bytes());
        }
        paths
    });
    (base, h)
}

#[test]
fn login_without_a_method_signs_in_through_the_browser_flow_and_stores_the_token() {
    let sb = Sandbox::new();
    let log = install_helper(&sb);
    let (hub, served) = device_hub(TOKEN);
    let out = asc(&sb, &["login", "--host", &hub], None, true);
    assert!(out.status.success(), "{}", text(&out.stderr));
    never_leaks(&out);
    let err = text(&out.stderr);
    assert!(err.contains(&format!("{hub}/login/device?user_code=WXYZ-1234")), "{err}");
    assert!(err.contains("WXYZ-1234"), "{err}");
    assert_eq!(served.join().unwrap(), ["/login/device/code", "/login/device/token", "/login/device/token"]);
    let log = calls(&log);
    assert!(log.contains("== store"), "{log}");
    assert!(log.contains(&format!("password={TOKEN}
")), "{log}");
    assert!(log.contains("username=asc
"), "{log}");
}

#[test]
fn the_login_methods_exclude_each_other() {
    let sb = Sandbox::new();
    let log = install_helper(&sb);
    let out = asc(&sb, &["login", "--host", "https://hub.example.com", "--device", "--token-stdin"], Some(TOKEN), false);
    assert!(!out.status.success());
    let out = asc(&sb, &["login", "--host", "https://hub.example.com", "--prompt", "--device"], None, false);
    assert!(!out.status.success());
    assert_eq!(calls(&log), "");
}
