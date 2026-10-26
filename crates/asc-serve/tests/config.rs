//! The `asc-serve.toml` that `deploy/coolify/asc-hub.compose.yml` renders (ASC Hub, all in one).
//!
//! The `hub-init` service writes the file with a shell heredoc from a few variables. This test
//! takes that heredoc from the compose file itself, fills the variables in the two shapes the
//! compose supports (SQLite + file store, and Postgres + S3) and runs the result through the same
//! parser and validation `asc serve` uses at start.

#![cfg(feature = "forgejo")]

use std::path::PathBuf;

use asc_core::transport_policy::set_config_allow_loopback_http;
use asc_serve::config::{AuthProvider, ServeConfig};

/// The heredoc body of `hub-init`, with the compose `$$` escapes resolved to `$`.
fn heredoc() -> String {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/coolify/asc-hub.compose.yml");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut lines = text
        .lines()
        .skip_while(|l| !l.contains("> \"$$CFG/asc-serve.toml.tmp\" <<EOF"));
    assert!(
        lines.next().is_some(),
        "the asc-serve.toml heredoc is gone from the compose file"
    );
    let body: Vec<&str> = lines
        .take_while(|l| l.trim() != "EOF")
        .map(str::trim_start)
        .collect();
    assert!(!body.is_empty());
    body.join("\n").replace("$$", "$")
}

/// Expands the shell variables of the heredoc the way `hub-init` does.
fn render(vars: &[(&str, &str)]) -> String {
    let mut out = heredoc();
    for (name, value) in vars {
        out = out.replace(&format!("${name}"), value);
    }
    assert!(!out.contains('$'), "unexpanded variable in:\n{out}");
    out
}

const SQLITE: &[(&str, &str)] = &[
    ("STORE", "file:///var/lib/asc/objects"),
    ("DB", "sqlite:///var/lib/asc/asc.db?mode=rwc"),
    ("ASC_CACHE_MEMORY", "256MiB"),
    ("ASC_CACHE_DISK", "2GiB"),
    ("ASC_TEXT_MEMORY", "256MiB"),
    ("ASC_GIT_ENABLED", "false"),
];

const S3_POSTGRES: &[(&str, &str)] = &[
    (
        "STORE",
        "s3://asc-hub?endpoint=https://garage.example.org&region=garage&path_style=true",
    ),
    (
        "DB",
        "postgres://asc_hub:secret@db.example.org:5432/hub?options=-csearch_path%3Dasc_hub",
    ),
    ("ASC_CACHE_MEMORY", "256MiB"),
    ("ASC_CACHE_DISK", "2GiB"),
    ("ASC_TEXT_MEMORY", "256MiB"),
    ("ASC_GIT_ENABLED", "true"),
];

#[test]
fn the_asc_hub_config_parses_and_validates() {
    let sqlite = render(SQLITE);
    let s3 = render(S3_POSTGRES);

    // `[forgejo] url` is plain http on loopback and the listener has no `[tls]` or https
    // `public_url`: both are refused unless ASC_ALLOW_LOOPBACK_HTTP=1, which the compose sets on
    // the asc containers. Checked first, while the process opt-in is still off.
    let err = ServeConfig::parse(&sqlite)
        .expect_err("strict policy must refuse the loopback forgejo url");
    assert!(err.to_string().contains("forgejo.url"), "{err}");

    set_config_allow_loopback_http(true);

    let cfg = ServeConfig::parse(&sqlite).expect("sqlite + file store");
    cfg.validate_listener()
        .expect("loopback listener under the opt-in");
    assert_eq!(cfg.listen.to_string(), "127.0.0.1:7890");
    assert_eq!(cfg.store.as_deref(), Some("file:///var/lib/asc/objects"));
    assert!(
        cfg.db
            .as_deref()
            .unwrap()
            .starts_with("sqlite:///var/lib/asc/asc.db")
    );
    assert_eq!(cfg.auth_provider(), Some(AuthProvider::Forgejo));
    assert_eq!(cfg.forge_url().as_deref(), Some("http://127.0.0.1:3000"));
    assert_eq!(
        cfg.import_repo_root(),
        Some(PathBuf::from("/var/lib/gitea/git/repositories"))
    );
    let forgejo = cfg.forgejo.as_ref().unwrap();
    assert_eq!(
        forgejo.internal_token_file,
        Some(PathBuf::from("/run/asc-hub/internal-token"))
    );
    assert_eq!(
        forgejo.webhook_secret_file,
        Some(PathBuf::from("/run/asc-hub/webhook-secret"))
    );
    assert!(cfg.limits.trusted_proxies.is_empty());
    assert!(!cfg.git.enabled);
    assert!(
        !cfg.ssh.enabled,
        "no SSH listener: the hub serves git over SSH"
    );

    let cfg = ServeConfig::parse(&s3).expect("postgres + s3 store");
    assert!(
        cfg.store
            .as_deref()
            .unwrap()
            .starts_with("s3://asc-hub?endpoint=https://")
    );
    assert!(cfg.db.as_deref().unwrap().starts_with("postgres://"));
    assert!(cfg.git.enabled);
}

#[test]
fn the_compose_file_wires_the_hub_to_the_same_places() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/coolify/asc-hub.compose.yml");
    let text = std::fs::read_to_string(path).unwrap();
    // The hub never mounts the volume that holds asc-serve.toml (the asc database URL).
    let hub = text
        .split("x-hub: &hub")
        .nth(1)
        .and_then(|t| t.split("x-asc: &asc").next())
        .expect("the hub anchor");
    assert!(hub.contains("hub-secrets:/run/asc-hub:ro"), "{hub}");
    assert!(!hub.contains("asc-runtime"), "{hub}");
    for service in ["  hub:\n", "  hub-admin:\n"] {
        let section = text.split(service).nth(1).unwrap_or_default();
        let section = section.split("\n  asc").next().unwrap_or_default();
        assert!(
            !section.contains("asc-runtime") && !section.contains("asc-serve.toml"),
            "{service}"
        );
    }
    for needle in [
        "ASC_SERVE_CONFIG: /run/asc-runtime/asc-serve.toml",
        "asc-runtime:/run/asc-runtime:ro",
        // the hub dials asc on loopback, asc and the hub share one network namespace
        "FORGEJO__asc__SERVER_URL: http://127.0.0.1:7890",
        "network_mode: service:hub",
        // the opt-in the loopback http above needs
        "ASC_ALLOW_LOOPBACK_HTTP: '1'",
        // the hub reads the same token asc reads
        "FORGEJO__security__INTERNAL_TOKEN_URI: file:///run/asc-hub/internal-token",
        "FORGEJO__asc__WRITER: asc",
        "FORGEJO__asc__NEW_REPOSITORIES: asc",
        // asc reads the hub's repositories at the hub's own path
        "FORGEJO__repository__ROOT: /var/lib/gitea/git/repositories",
        "hub-repos:/var/lib/gitea/git/repositories:ro",
    ] {
        assert!(text.contains(needle), "compose file lacks {needle:?}");
    }
}
