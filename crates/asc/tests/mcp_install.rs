//! M09b acceptance: `asc mcp install` (SPEC 22.8). Project scope writes only narrowing flags,
//! user scope writes config keys, and the HTTP endpoint is never added next to `asc mcp`.

mod support;

use std::path::PathBuf;

use serde_json::{Value, json};
use support::{Sandbox, text};

struct Repo {
    sb: Sandbox,
    root: PathBuf,
}

fn repo() -> Repo {
    let sb = Sandbox::new();
    let root = sb.dir("repo");
    let r = Repo { sb, root };
    let out = r.run(&["init", "-q", "."]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    r
}

impl Repo {
    fn run(&self, args: &[&str]) -> std::process::Output {
        let mut c = self.sb.asc(&self.root);
        c.args(args).env("ASC_NO_DAEMON", "1");
        c.output().expect("spawn asc")
    }

    fn mcp_json(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.root.join(".mcp.json")).unwrap()).unwrap()
    }
}

#[test]
fn project_scope_writes_only_narrowing_flags() {
    let r = repo();
    let out = r.run(&["mcp", "install"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let doc = r.mcp_json();
    assert_eq!(
        doc["mcpServers"]["asc"],
        json!({"command": "asc", "args": ["mcp", "--read-only"]})
    );
    let raw = std::fs::read_to_string(r.root.join(".mcp.json")).unwrap();
    assert!(!raw.contains("--write"), "{raw}");
    // Project scope never touches the user config.
    let gitconfig = std::fs::read_to_string(r.sb.user.join(".gitconfig")).unwrap();
    assert!(!gitconfig.contains("mcp"), "{gitconfig}");
}

#[test]
fn project_scope_merges_and_is_idempotent() {
    let r = repo();
    std::fs::write(
        r.root.join(".mcp.json"),
        br#"{"mcpServers":{"other":{"command":"x"}},"extra":true}"#,
    )
    .unwrap();
    for _ in 0..2 {
        assert!(r.run(&["mcp", "install", "--scope", "project"]).status.success());
    }
    let doc = r.mcp_json();
    assert_eq!(doc["mcpServers"]["other"]["command"], "x");
    assert_eq!(doc["extra"], true);
    assert_eq!(doc["mcpServers"]["asc"]["args"], json!(["mcp", "--read-only"]));
}

#[test]
fn remote_entry_is_https_only_and_not_doubled() {
    let r = repo();
    for bad in [
        "http://hub.example",
        "https://user:secret@hub.example",
        "https://hub.example/?token=abc",
    ] {
        let out = r.run(&["mcp", "install", "--remote", bad]);
        assert!(!out.status.success(), "{bad}");
        assert!(!r.root.join(".mcp.json").exists());
        assert!(!text(&out.stderr).contains("secret"), "{}", text(&out.stderr));
    }
    assert!(
        r.run(&["mcp", "install", "--remote", "https://hub.example"])
            .status
            .success()
    );
    assert_eq!(
        r.mcp_json()["mcpServers"]["asc"]["args"],
        json!(["mcp", "--remote", "https://hub.example", "--read-only"])
    );

    // An existing HTTP entry for the same endpoint blocks the install and the file stays as is.
    let before = br#"{"mcpServers":{"hub":{"type":"http","url":"https://hub.example/mcp"}}}"#;
    std::fs::write(r.root.join(".mcp.json"), before).unwrap();
    let out = r.run(&["mcp", "install", "--remote", "https://hub.example"]);
    assert!(!out.status.success());
    assert_eq!(std::fs::read(r.root.join(".mcp.json")).unwrap(), before);
}

#[test]
fn malformed_file_is_not_clobbered() {
    let r = repo();
    std::fs::write(r.root.join(".mcp.json"), b"{ not json").unwrap();
    let out = r.run(&["mcp", "install"]);
    assert!(!out.status.success());
    assert_eq!(std::fs::read(r.root.join(".mcp.json")).unwrap(), b"{ not json");
}

#[test]
fn symlinked_file_is_refused() {
    let r = repo();
    let target = r.sb.dir("elsewhere").join("victim.json");
    std::fs::write(&target, b"{}").unwrap();
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&target, r.root.join(".mcp.json")).is_ok();
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(&target, r.root.join(".mcp.json")).is_ok();
    if !linked {
        return; // Windows without the symlink privilege.
    }
    let out = r.run(&["mcp", "install"]);
    assert!(!out.status.success());
    assert_eq!(std::fs::read(&target).unwrap(), b"{}");
}

#[test]
fn user_scope_writes_keys_and_never_widens() {
    let r = repo();
    let cfg = r.sb.user.join(".gitconfig");
    std::fs::write(&cfg, "[asc \"mcp\"]\n\twrite = local\n").unwrap();
    let out = r.run(&["mcp", "install", "--scope", "user"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let after = std::fs::read_to_string(&cfg).unwrap();
    assert!(after.starts_with("[asc \"mcp\"]\n\twrite = local\n"), "{after}");
    assert!(after.contains("toolsets = core") && after.contains("confirm = destructive"));
    assert!(!after.contains("write = false"), "{after}");
    // The project is untouched, and a second run changes nothing.
    assert!(!r.root.join(".mcp.json").exists());
    assert!(r.run(&["mcp", "install", "--scope", "user"]).status.success());
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), after);
    // --remote has no meaning for the user scope.
    assert!(
        !r.run(&["mcp", "install", "--scope", "user", "--remote", "https://h.example"])
            .status
            .success()
    );
}
