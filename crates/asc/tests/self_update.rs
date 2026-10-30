//! `asc self-update` and `asc --version` through the built binary: the release source (a
//! directory, `ASC_DOWNLOAD_BASE_URL` over loopback http), the https rule, and the version line.

mod support;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;

use support::{Sandbox, text};

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run(sb: &Sandbox, cwd: &Path, args: &[&str], env: &[(&str, &str)], unset: &[&str]) -> Run {
    let mut cmd = sb.asc(cwd);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    for k in unset {
        cmd.env_remove(k);
    }
    let out = cmd.output().expect("spawn asc");
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: text(&out.stdout),
        stderr: text(&out.stderr),
    }
}

/// A release directory whose `latest/version` holds `content`.
fn releases(sb: &Sandbox, name: &str, content: &str) -> std::path::PathBuf {
    let dir = sb.dir(name);
    std::fs::create_dir_all(dir.join("latest")).unwrap();
    std::fs::write(dir.join("latest").join("version"), content).unwrap();
    dir
}

/// Serves `body` for `/r/latest/version` on the loopback interface (and 404 for the rest) until the
/// returned base URL has been asked `n` times.
fn serve_latest(body: &'static str, n: usize) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/r", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let mut paths = Vec::new();
        for _ in 0..n {
            let (mut sock, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(sock.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).unwrap() == 0 || h == "\r\n" || h == "\n" {
                    break;
                }
            }
            let (status, payload) = if path == "/r/latest/version" {
                ("200 OK", body)
            } else {
                ("404 Not Found", "")
            };
            write!(
                sock,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            )
            .unwrap();
            paths.push(path);
        }
        paths
    });
    (base, handle)
}

#[test]
fn version_line_has_the_commit() {
    let sb = Sandbox::new();
    let dir = sb.dir("d");
    let out = run(&sb, &dir, &["--version"], &[], &[]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let re = regex::Regex::new(
        r"^asc \d+\.\d+\.\d+ \((git \d+\.\d+\S*, )?commit ([0-9a-f]{7,12}|unknown)\)\n$",
    )
    .unwrap();
    assert!(re.is_match(&out.stdout), "{}", out.stdout);
}

#[test]
fn the_environment_names_the_commit() {
    // The release workflow sets ASC_BUILD_COMMIT; build.rs embeds it only when it is hex.
    let commit = env!("ASC_BUILD_COMMIT");
    assert!(
        commit == "unknown" || commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "{commit}"
    );
}

#[test]
fn check_from_a_directory_reports_up_to_date_and_newer() {
    let sb = Sandbox::new();
    let cwd = sb.dir("cwd");
    let version = env!("CARGO_PKG_VERSION");
    let same = releases(&sb, "same", &format!("{version}\n"));
    let out = run(
        &sb,
        &cwd,
        &["self-update", "--check", "--from", same.to_str().unwrap()],
        &[],
        &[],
    );
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, format!("asc {version} is up to date\n").as_str()),
        "{}",
        out.stderr
    );

    let newer = releases(&sb, "newer", "99.0.0\n");
    let out = run(
        &sb,
        &cwd,
        &["self-update", "--check", "--from", newer.to_str().unwrap()],
        &[],
        &[],
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.stdout, "hint: asc 99.0.0 is available (asc upgrade)\n");
}

#[test]
fn a_latest_file_that_is_not_a_version_is_refused() {
    let sb = Sandbox::new();
    let cwd = sb.dir("cwd");
    for junk in ["../../etc\n", "1.2\n", "1.2.3-rc1\n", ""] {
        let dir = releases(&sb, "junk", junk);
        let out = run(
            &sb,
            &cwd,
            &["self-update", "--check", "--from", dir.to_str().unwrap()],
            &[],
            &[],
        );
        assert_ne!(out.code, 0, "{junk:?}");
        assert!(
            out.stderr.contains("not a version"),
            "{junk:?}: {}",
            out.stderr
        );
    }
}

#[test]
fn the_download_base_url_is_the_source_when_nothing_else_is_set() {
    let sb = Sandbox::new();
    let cwd = sb.dir("cwd");
    let (base, server) = serve_latest("99.1.0\n", 1);
    let out = run(
        &sb,
        &cwd,
        &["self-update", "--check"],
        &[("ASC_DOWNLOAD_BASE_URL", &base)],
        &[],
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.stdout, "hint: asc 99.1.0 is available (asc upgrade)\n");
    assert_eq!(server.join().unwrap(), ["/r/latest/version"]);
}

#[test]
fn plain_http_needs_the_loopback_opt_in() {
    let sb = Sandbox::new();
    let cwd = sb.dir("cwd");
    // Not loopback: refused before any request is made.
    let out = run(
        &sb,
        &cwd,
        &["self-update", "--check"],
        &[("ASC_DOWNLOAD_BASE_URL", "http://releases.example/r")],
        &[],
    );
    assert_ne!(out.code, 0);
    assert!(out.stderr.contains("https"), "{}", out.stderr);
    // Loopback without the opt-in: refused too.
    let out = run(
        &sb,
        &cwd,
        &["self-update", "--check"],
        &[("ASC_DOWNLOAD_BASE_URL", "http://127.0.0.1:9/r")],
        &["ASC_ALLOW_LOOPBACK_HTTP"],
    );
    assert_ne!(out.code, 0);
    assert!(out.stderr.contains("asc.releasesUrl"), "{}", out.stderr);
}

#[test]
fn without_a_source_it_says_how_to_set_one() {
    let sb = Sandbox::new();
    let cwd = sb.dir("cwd");
    let out = run(&sb, &cwd, &["self-update", "--check"], &[], &[]);
    assert_ne!(out.code, 0);
    assert!(
        out.stderr.contains("ASC_DOWNLOAD_BASE_URL") && out.stderr.contains("--from"),
        "{}",
        out.stderr
    );
}

#[test]
fn a_secret_in_the_base_url_is_not_echoed() {
    let sb = Sandbox::new();
    let cwd = sb.dir("cwd");
    let out = run(
        &sb,
        &cwd,
        &["self-update", "--check"],
        &[(
            "ASC_DOWNLOAD_BASE_URL",
            "http://user:hunter2@releases.example/r?t=sekret",
        )],
        &[],
    );
    assert_ne!(out.code, 0);
    assert!(
        !out.stderr.contains("hunter2") && !out.stderr.contains("sekret"),
        "{}",
        out.stderr
    );
}
