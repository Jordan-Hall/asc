use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use super::*;

fn check(level: Level, name: &str, detail: &str) -> Check {
    Check::new(level, name, detail)
}

#[test]
fn names_are_cut_to_what_the_hub_takes() {
    assert_eq!(report_name("real-git").as_deref(), Some("real-git"));
    assert_eq!(report_name("git@bash").as_deref(), Some("git-bash"));
    assert_eq!(report_name("Long Paths").as_deref(), Some("long-paths"));
    assert_eq!(report_name("--x").as_deref(), Some("x"));
    assert_eq!(report_name("@@"), None);
    assert_eq!(report_name(&"a".repeat(90)).map(|n| n.len()), Some(40));
}

#[test]
fn the_git_version_has_no_path() {
    assert_eq!(
        git_version("git version 2.47.1.windows.1 at C:/Program Files/Git/cmd/git.exe").as_deref(),
        Some("git version 2.47.1.windows.1")
    );
    assert_eq!(
        git_version("git version 2.30.0 at /usr/bin/git is older than 2.40").as_deref(),
        Some("git version 2.30.0")
    );
    assert_eq!(git_version("cannot read the version of /usr/bin/git"), None);
    assert_eq!(report_text("1.0 <b>\u{7}"), "1.0 b");
}

#[test]
fn the_body_holds_names_and_levels_only() {
    let checks = vec![
        check(
            Level::Ok,
            "real-git",
            "git version 2.47.1 at /home/me/bin/git",
        ),
        check(Level::Ok, "remote", "https://hub.example/a exists"),
        check(
            Level::Warn,
            "remote",
            "https://hub.example/b is not reachable",
        ),
        check(
            Level::Warn,
            "git@bash",
            "resolves to /usr/bin/git, not the asc shim",
        ),
        check(Level::Fail, "token", "rejected (credential from netrc)"),
    ];
    let b = body(&checks);
    assert_eq!(b["git"], "git version 2.47.1");
    assert_eq!(b["os"], std::env::consts::OS);
    assert_eq!(b["asc"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        b["checks"],
        serde_json::json!([
            {"name": "real-git", "level": "ok"},
            {"name": "remote", "level": "warn"},
            {"name": "git-bash", "level": "warn"},
            {"name": "token", "level": "fail"},
        ]),
        "each name once at its worst level"
    );
    let text = b.to_string();
    assert!(!text.contains("/home/me") && !text.contains("hub.example") && !text.contains("netrc"));

    let many: Vec<Check> = (0..100)
        .map(|i| check(Level::Ok, &format!("c{i}"), ""))
        .collect();
    assert_eq!(body(&many)["checks"].as_array().unwrap().len(), MAX_CHECKS);
}

#[test]
fn the_endpoint_is_https_under_the_hub() {
    assert_eq!(
        endpoint("https://hub.example").unwrap().as_str(),
        "https://hub.example/api/v1/user/asc/doctor"
    );
    assert_eq!(
        endpoint("https://hub.example/sub?x=1#f").unwrap().as_str(),
        "https://hub.example/sub/api/v1/user/asc/doctor"
    );
    assert_eq!(
        endpoint("asc+https://hub.example/").unwrap().as_str(),
        "https://hub.example/api/v1/user/asc/doctor"
    );
    assert!(endpoint("http://hub.example").is_err());
    assert!(endpoint("https://me:pw@hub.example").is_err());
    assert!(endpoint("ftp://hub.example").is_err());
    assert!(endpoint("not a url").is_err());
}

/// Answers each request with the next status and keeps the request text.
fn serve(l: TcpListener, statuses: Vec<u16>) -> Arc<Mutex<Vec<String>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    std::thread::spawn(move || {
        for status in statuses {
            let Ok((mut c, _)) = l.accept() else { return };
            let mut buf = vec![0u8; 16 * 1024];
            let mut n = 0;
            loop {
                let r = c.read(&mut buf[n..]).unwrap_or(0);
                n += r;
                let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                if let Some(h) = text.find("\r\n\r\n") {
                    let len = text[..h]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if n >= h + 4 + len || r == 0 {
                        s2.lock().unwrap().push(text);
                        break;
                    }
                }
                if r == 0 {
                    break;
                }
            }
            let resp =
                format!("HTTP/1.1 {status} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            let _ = c.write_all(resp.as_bytes());
        }
    });
    seen
}

#[tokio::test]
async fn send_posts_with_the_credential_and_names_each_refusal() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let url = base.join(PATH).unwrap();
    let seen = serve(listener, vec![204, 401, 403, 404, 422, 500]);
    let config = gix_config::File::default();
    let resolved = Resolver::new(&config)
        .without_helpers()
        .with_token(&url, "tok_secret".into(), Source::Store, false)
        .unwrap();
    let client = reqwest::Client::new();
    let b = body(&[check(Level::Ok, "real-git", "git version 2.47.1 at /x")]);

    send(&client, &url, &resolved, &b).await.unwrap();
    let mut errors = Vec::new();
    for _ in 0..5 {
        errors.push(send(&client, &url, &resolved, &b).await.unwrap_err());
    }
    assert!(errors[0].contains("sign in again with asc auth login --host http://127.0.0.1:"));
    assert!(errors[1].contains("repository scope"));
    assert!(errors[2].contains("takes no doctor report"));
    assert!(errors[3].contains("not valid"));
    assert_eq!(errors[4], "the hub answered 500");
    assert!(errors.iter().all(|e| !e.contains("tok_secret")));

    let seen = seen.lock().unwrap();
    let first = &seen[0];
    assert!(first.starts_with("POST /api/v1/user/asc/doctor HTTP/1.1\r\n"));
    let lower = first.to_ascii_lowercase();
    assert!(
        lower.contains("\r\nauthorization: basic "),
        "the stored token is sent"
    );
    assert!(lower.contains("\r\ncontent-type: application/json\r\n"));
    let json = &first[first.find("\r\n\r\n").unwrap() + 4..];
    assert_eq!(serde_json::from_str::<serde_json::Value>(json).unwrap(), b);
}

#[tokio::test]
async fn an_unreachable_hub_is_named_without_the_credential() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = Url::parse(&format!("http://{}/{PATH}", listener.local_addr().unwrap())).unwrap();
    drop(listener);
    let config = gix_config::File::default();
    let resolved = Resolver::new(&config)
        .without_helpers()
        .with_token(&url, "tok_secret".into(), Source::Store, false)
        .unwrap();
    let e = send(
        &reqwest::Client::new(),
        &url,
        &resolved,
        &serde_json::json!({}),
    )
    .await
    .unwrap_err();
    assert!(e.starts_with("the hub could not be reached"));
    assert!(!e.contains("tok_secret"));
}
