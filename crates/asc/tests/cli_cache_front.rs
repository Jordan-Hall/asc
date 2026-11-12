//! The build cache fronts under `asc cache serve`: each tool's protocol stores and returns
//! entries in the shared store, a request without the token or with another host is refused,
//! and `asc cache env` and `asc cache setup` name the same port and token.

mod support;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::Digest;
use support::{Sandbox, asc_binary, text};

fn ok(sb: &Sandbox, cwd: &Path, args: &[&str]) -> String {
    let out = sb.asc(cwd).args(args).output().expect("spawn asc");
    assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
    text(&out.stdout)
}

struct Reply {
    status: u16,
    headers: String,
    body: Vec<u8>,
}

/// One HTTP/1.1 request; `host` and `auth` are sent as given.
fn http(
    port: u16,
    method: &str,
    path: &str,
    host: &str,
    auth: Option<&str>,
    extra: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Reply {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    if let Some(a) = auth {
        req.push_str(&format!("Authorization: {a}\r\n"));
    }
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = body {
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).unwrap();
    if let Some(b) = body {
        s.write_all(b).unwrap();
    }
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a reply head");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status code");
    let mut body = raw[split + 4..].to_vec();
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        body = unchunk(&body);
    }
    Reply {
        status,
        headers: head,
        body,
    }
}

fn unchunk(mut raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let eol = raw.windows(2).position(|w| w == b"\r\n").unwrap();
        let size = usize::from_str_radix(
            String::from_utf8_lossy(&raw[..eol])
                .split(';')
                .next()
                .unwrap()
                .trim(),
            16,
        )
        .unwrap();
        raw = &raw[eol + 2..];
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&raw[..size]);
        raw = &raw[size + 2..];
    }
}

/// `asc cache serve`, stopped when dropped.
struct Served(Child);

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn serve(sb: &Sandbox, cwd: &Path, port: u16) -> Served {
    let child = sb
        .asc(cwd)
        .args(["cache", "serve"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn asc cache serve");
    let served = Served(child);
    let start = Instant::now();
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "the fronts did not start"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    served
}

fn entries(sb: &Sandbox, cwd: &Path, tool: &str) -> usize {
    let v: Value =
        serde_json::from_str(&ok(sb, cwd, &["cache", "ls", "--tool", tool, "--json"])).unwrap();
    v.as_array().unwrap().len()
}

#[test]
fn every_front_stores_and_returns_entries_behind_the_token() {
    if asc_binary().is_none() {
        return;
    }
    let sb = Sandbox::new();
    let repo = sb.dir("repo");
    ok(&sb, &repo, &["init", "-q"]);
    ok(&sb, &repo, &["config", "asc.buildcache.remote", "off"]);
    let env: Value = serde_json::from_str(&ok(&sb, &repo, &["cache", "env", "--shell", "json"]))
        .expect("env as JSON");
    let url = env["TURBO_API"].as_str().unwrap().to_owned();
    let token = env["TURBO_TOKEN"].as_str().unwrap().to_owned();
    assert_eq!(env["NX_SELF_HOSTED_REMOTE_CACHE_SERVER"], url.as_str());
    assert_eq!(
        env["NX_SELF_HOSTED_REMOTE_CACHE_ACCESS_TOKEN"],
        token.as_str()
    );
    assert!(
        env["MAVEN_ARGS"]
            .as_str()
            .unwrap()
            .contains(&format!("remote.url={url}/maven")),
        "{env}"
    );
    let port: u16 = url.rsplit(':').next().unwrap().parse().unwrap();
    // A second call keeps the port and token.
    let again: Value =
        serde_json::from_str(&ok(&sb, &repo, &["cache", "env", "--shell", "json"])).unwrap();
    assert_eq!(again["TURBO_TOKEN"], token.as_str());
    // In a work tree turbo's team is the tree's, recorded for the daemon's server tier.
    let team = env["TURBO_TEAM"].as_str().unwrap().to_owned();
    assert!(team.starts_with("asc-") && team.len() == 20, "{team}");
    assert_eq!(again["TURBO_TEAM"], team.as_str());
    let teams: Value = serde_json::from_slice(
        &std::fs::read(sb.asc_home.join("buildcache-teams.json")).expect("teams file"),
    )
    .unwrap();
    assert!(teams["teams"][&team]["root"].is_string(), "{teams}");
    let outside = sb.dir("outside");
    let plain: Value =
        serde_json::from_str(&ok(&sb, &outside, &["cache", "env", "--shell", "json"])).unwrap();
    assert_eq!(plain["TURBO_TEAM"], "asc");

    let _served = serve(&sb, &repo, port);
    let host = format!("127.0.0.1:{port}");
    let bearer = format!("Bearer {token}");
    let auth = Some(bearer.as_str());
    use base64::Engine;
    let basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("asc:{token}"))
    );

    // The token and a loopback host are required.
    let r = http(port, "GET", "/v1/cache/abc", &host, None, &[], None);
    assert_eq!(r.status, 401);
    assert!(r.headers.to_ascii_lowercase().contains("www-authenticate"));
    let r = http(
        port,
        "GET",
        "/v1/cache/abc",
        &host,
        Some("Bearer wrong"),
        &[],
        None,
    );
    assert_eq!(r.status, 401);
    let r = http(
        port,
        "GET",
        "/v1/cache/abc",
        &format!("evil.example.org:{port}"),
        auth,
        &[],
        None,
    );
    assert_eq!(r.status, 403);

    // Nx: a miss, a store, a hit, and a second store of the same hash refused.
    assert_eq!(
        http(port, "GET", "/v1/cache/n1", &host, auth, &[], None).status,
        404
    );
    assert_eq!(
        http(
            port,
            "PUT",
            "/v1/cache/n1",
            &host,
            auth,
            &[],
            Some(b"nx out")
        )
        .status,
        200
    );
    let r = http(port, "GET", "/v1/cache/n1", &host, auth, &[], None);
    assert_eq!((r.status, r.body.as_slice()), (200, &b"nx out"[..]));
    assert_eq!(
        http(
            port,
            "PUT",
            "/v1/cache/n1",
            &host,
            auth,
            &[],
            Some(b"again")
        )
        .status,
        409
    );

    // Turbo: the artefact keeps its signature tag.
    let r = http(
        port,
        "PUT",
        "/v8/artifacts/t1?teamId=asc",
        &host,
        auth,
        &[("x-artifact-tag", "sig-1")],
        Some(b"turbo out"),
    );
    assert_eq!(r.status, 200);
    assert!(String::from_utf8_lossy(&r.body).contains("urls"));
    let r = http(port, "GET", "/v8/artifacts/t1", &host, auth, &[], None);
    assert_eq!((r.status, r.body.as_slice()), (200, &b"turbo out"[..]));
    assert!(
        r.headers
            .to_ascii_lowercase()
            .contains("x-artifact-tag: sig-1"),
        "{}",
        r.headers
    );
    let r = http(port, "GET", "/v8/artifacts/status", &host, auth, &[], None);
    assert_eq!(r.status, 200);

    // Bazel: the CAS checks the digest; the action cache does not.
    let blob = b"bazel blob";
    let digest = hex(&sha2::Sha256::digest(blob));
    let wrong = hex(&sha2::Sha256::digest(b"other"));
    let cas = format!("/bazel/cas/{digest}");
    assert_eq!(
        http(port, "PUT", &cas, &host, auth, &[], Some(blob)).status,
        200
    );
    assert_eq!(
        http(
            port,
            "PUT",
            &format!("/bazel/cas/{wrong}"),
            &host,
            auth,
            &[],
            Some(blob)
        )
        .status,
        400
    );
    let r = http(port, "GET", &cas, &host, auth, &[], None);
    assert_eq!((r.status, r.body.as_slice()), (200, &blob[..]));
    let ac = format!("/bazel/ac/{wrong}");
    assert_eq!(
        http(port, "PUT", &ac, &host, auth, &[], Some(b"result")).status,
        200
    );
    assert_eq!(http(port, "HEAD", &ac, &host, auth, &[], None).status, 200);

    // Gradle and Maven with Basic credentials.
    let b = Some(basic.as_str());
    assert_eq!(
        http(port, "PUT", "/gradle/g1", &host, b, &[], Some(b"gradle")).status,
        200
    );
    let r = http(port, "GET", "/gradle/g1", &host, b, &[], None);
    assert_eq!((r.status, r.body.as_slice()), (200, &b"gradle"[..]));
    let m = "/maven/v1/org.example/app/build-1/buildinfo.xml";
    assert_eq!(
        http(port, "PUT", m, &host, b, &[], Some(b"<build/>")).status,
        200
    );
    assert_eq!(http(port, "GET", m, &host, b, &[], None).body, b"<build/>");
    assert_eq!(
        http(port, "GET", "/maven/v1/../secret", &host, b, &[], None).status,
        400
    );

    // ccache: store, read, delete.
    let c = "/ccache/ab/cdef0123";
    assert_eq!(
        http(port, "PUT", c, &host, b, &[], Some(b"obj")).status,
        200
    );
    assert_eq!(http(port, "GET", c, &host, b, &[], None).body, b"obj");
    assert_eq!(http(port, "DELETE", c, &host, b, &[], None).status, 200);
    assert_eq!(http(port, "GET", c, &host, b, &[], None).status, 404);

    // Every entry is in the shared store, under its tool.
    assert_eq!(entries(&sb, &repo, "nx"), 1);
    assert_eq!(entries(&sb, &repo, "turbo"), 1);
    assert_eq!(entries(&sb, &repo, "bazel"), 2);
    assert_eq!(entries(&sb, &repo, "gradle"), 1);
    assert_eq!(entries(&sb, &repo, "maven"), 1);
    assert_eq!(entries(&sb, &repo, "ccache"), 0);
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn setup_writes_and_undoes_the_bazel_gradle_and_maven_files() {
    if asc_binary().is_none() {
        return;
    }
    let sb = Sandbox::new();
    let repo = sb.dir("repo");
    ok(&sb, &repo, &["init", "-q"]);
    let user = sb.dir("user");
    let gradle_home = sb.dir("gradle-home");
    let setup = |args: &[&str]| {
        let out = sb
            .asc(&repo)
            .env("HOME", &user)
            .env("USERPROFILE", &user)
            .env("GRADLE_USER_HOME", &gradle_home)
            .args(["cache", "setup"])
            .args(args)
            .output()
            .expect("spawn asc");
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    };
    let env: Value =
        serde_json::from_str(&ok(&sb, &repo, &["cache", "env", "--shell", "json"])).unwrap();
    let token = env["TURBO_TOKEN"].as_str().unwrap().to_owned();
    let url = env["TURBO_API"].as_str().unwrap().to_owned();

    // Bazel: a marked block next to what the user has.
    std::fs::write(user.join(".bazelrc"), "build --jobs=4\n").unwrap();
    assert!(setup(&["bazel"]).contains("set build --remote_cache"));
    let rc = std::fs::read_to_string(user.join(".bazelrc")).unwrap();
    assert!(rc.starts_with("build --jobs=4\n"), "{rc}");
    assert!(rc.contains(&format!("--remote_cache={url}/bazel")), "{rc}");
    assert!(
        rc.contains(&format!("Authorization=Bearer {token}")),
        "{rc}"
    );
    assert!(setup(&["bazel"]).contains("nothing to change"));
    assert!(setup(&["bazel", "--undo"]).contains("removed"));
    assert_eq!(
        std::fs::read_to_string(user.join(".bazelrc")).unwrap(),
        "build --jobs=4\n"
    );

    // Gradle: the init script and org.gradle.caching.
    let said = setup(&["gradle"]);
    assert!(said.contains("set init script"), "{said}");
    let script = gradle_home.join("init.d/asc-build-cache.gradle");
    assert!(
        std::fs::read_to_string(&script)
            .unwrap()
            .contains("HttpBuildCache")
    );
    let props = std::fs::read_to_string(gradle_home.join("gradle.properties")).unwrap();
    assert!(props.contains("org.gradle.caching=true"), "{props}");
    setup(&["gradle", "--undo"]);
    assert!(!script.exists());
    assert!(!gradle_home.join("gradle.properties").exists());

    // Maven: the server entry inside the user's servers.
    let m2 = user.join(".m2");
    std::fs::create_dir_all(&m2).unwrap();
    let mine =
        "<settings>\n  <servers>\n    <server><id>mine</id></server>\n  </servers>\n</settings>\n";
    std::fs::write(m2.join("settings.xml"), mine).unwrap();
    assert!(setup(&["maven"]).contains("set server = asc-build-cache"));
    let xml = std::fs::read_to_string(m2.join("settings.xml")).unwrap();
    assert!(
        xml.contains("<id>mine</id>") && xml.contains("<id>asc-build-cache</id>"),
        "{xml}"
    );
    assert!(
        xml.contains(&format!("<password>{token}</password>")),
        "{xml}"
    );
    assert!(xml.find("asc-build-cache").unwrap() < xml.find("</servers>").unwrap());
    setup(&["maven", "--undo"]);
    assert_eq!(
        std::fs::read_to_string(m2.join("settings.xml")).unwrap(),
        mine
    );

    // The port and token are this machine's.
    let out = sb
        .asc(&repo)
        .args(["cache", "setup", "bazel", "--repo"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    // Turbo and Nx read the environment.
    assert!(setup(&["turbo"]).contains("asc cache env"));
}
