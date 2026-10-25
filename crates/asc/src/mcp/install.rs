//! `asc mcp install [--scope project|user] [--remote <url>]` (SPEC 22.8, unit M09b).
//!
//! * **project** (default) merges one `asc` entry into `<worktree>/.mcp.json`. A committed file is
//!   untrusted input to whoever opens the repository, so the entry carries narrowing flags only:
//!   always `--read-only`, never `--write`. Other servers in the file are kept. The file is
//!   replaced atomically and is never written through a symbolic link.
//! * **user** writes the `asc.mcp.*` keys into the user's global config instead of widening flags.
//!   Keys that are already set are left alone, so an install never changes a ceiling the user
//!   chose, up or down.
//!
//! The entry is the stdio server (`asc mcp`) or, with `--remote`, `asc mcp --remote <url>`. It never
//! registers the HTTP `/mcp` endpoint next to `asc mcp` for the same repository, so an agent never
//! sees a tool twice; an existing entry that does is an error. The remote URL must be `https`
//! (loopback `http` only with `ASC_ALLOW_LOOPBACK_HTTP`) and must carry no credentials, query or
//! fragment, because it lands in a file that is committed and in argv.

use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Map, Value, json};

use crate::cli::common::{CliError, Ctx, Result};

const USAGE: &str = "usage: asc mcp install [--scope project|user] [--remote <url>]";
const ENTRY: &str = "asc";

/// Where the install writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Project,
    User,
}

/// Parsed arguments of `asc mcp install`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub scope: Scope,
    pub remote: Option<String>,
}

fn usage() -> CliError {
    CliError::Usage(USAGE.to_owned())
}

fn fatal(msg: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("asc: mcp install: {msg}"))
}

/// Parses the arguments after `install`.
///
/// # Errors
/// A usage error for an unknown flag, a missing value or a bad scope, and a fatal error for a
/// `--remote` URL that is not acceptable.
pub fn parse(argv: &[OsString], loopback_http: bool) -> Result<Options> {
    let mut scope = None;
    let mut remote = None;
    let mut it = argv.iter().map(|a| a.to_string_lossy().into_owned());
    while let Some(a) = it.next() {
        let (name, inline) = match a.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_owned(), Some(v.to_owned())),
            _ => (a.clone(), None),
        };
        match name.as_str() {
            "--scope" if scope.is_none() => {
                let v = inline.or_else(|| it.next()).ok_or_else(usage)?;
                scope = Some(match v.as_str() {
                    "project" => Scope::Project,
                    "user" => Scope::User,
                    _ => return Err(usage()),
                });
            }
            "--remote" if remote.is_none() => {
                remote = Some(inline.or_else(|| it.next()).ok_or_else(usage)?);
            }
            _ => return Err(usage()),
        }
    }
    let scope = scope.unwrap_or(Scope::Project);
    if let Some(url) = &remote {
        if scope == Scope::User {
            return Err(CliError::Usage(format!(
                "{USAGE}\n--remote needs --scope project: the user scope only writes asc.mcp.* keys"
            )));
        }
        check_remote(url, loopback_http)?;
    }
    Ok(Options { scope, remote })
}

/// Accepts an `https` base URL with no credentials, query or fragment.
fn check_remote(raw: &str, loopback_http: bool) -> Result<()> {
    let url = url::Url::parse(raw).map_err(|_| fatal("--remote is not a valid URL"))?;
    let loopback = match url.host() {
        Some(url::Host::Domain(d)) => d == "localhost",
        Some(url::Host::Ipv4(a)) => a.is_loopback(),
        Some(url::Host::Ipv6(a)) => a.is_loopback(),
        None => false,
    };
    match url.scheme() {
        "https" => {}
        "http" if loopback && loopback_http => {}
        _ => return Err(fatal("--remote must be an https URL")),
    }
    if url.host_str().is_none() {
        return Err(fatal("--remote has no host"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(fatal(
            "--remote must not carry credentials; use the git credential helper",
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(fatal("--remote must not carry a query or fragment"));
    }
    Ok(())
}

/// The `.mcp.json` entry: narrowing flags only.
#[must_use]
pub fn entry(remote: Option<&str>) -> Value {
    let mut args = vec!["mcp".to_owned()];
    if let Some(r) = remote {
        args.push("--remote".to_owned());
        args.push(r.to_owned());
    }
    args.push("--read-only".to_owned());
    json!({ "command": "asc", "args": args })
}

fn norm(u: &str) -> String {
    let t = u.trim().trim_end_matches('/');
    t.strip_suffix("/mcp")
        .unwrap_or(t)
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

/// Merges the `asc` entry into the parsed `.mcp.json`.
///
/// # Errors
/// The document is not an object, or another entry already registers the HTTP endpoint of the
/// server being installed.
pub fn merge(doc: Option<Value>, remote: Option<&str>) -> Result<Value> {
    let mut root = match doc {
        None => Map::new(),
        Some(Value::Object(m)) => m,
        Some(_) => return Err(fatal(".mcp.json is not a JSON object; not changing it")),
    };
    let servers = root
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(servers) = servers else {
        return Err(fatal(
            ".mcp.json: mcpServers is not an object; not changing it",
        ));
    };
    if let Some(r) = remote {
        for (name, v) in servers.iter() {
            if name == ENTRY {
                continue;
            }
            if v.get("url")
                .and_then(Value::as_str)
                .is_some_and(|u| norm(u) == norm(r))
            {
                return Err(fatal(format!(
                    "entry {name:?} already registers the /mcp endpoint of {r}; remove it first so the tools are not listed twice"
                )));
            }
        }
    }
    servers.insert(ENTRY.to_owned(), entry(remote));
    Ok(Value::Object(root))
}

/// Writes `bytes` to `path` through a temporary file in the same directory.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().ok_or_else(|| fatal("no parent directory"))?;
    std::fs::create_dir_all(dir).map_err(fatal)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp = dir.join(format!(".asc-mcp-install.{}.{stamp}.tmp", std::process::id()));
    let written = (|| {
        // `create_new` never follows a pre-planted link or overwrites a file.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(fatal(e));
    }
    Ok(())
}

/// Refuses a symbolic link at `path`: a hostile repository could point it outside the tree.
fn refuse_link(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Err(fatal(format!(
            "{} is a symbolic link; not writing through it",
            path.display()
        ))),
        Ok(m) if !m.is_file() => Err(fatal(format!("{} is not a regular file", path.display()))),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(fatal(e)),
    }
}

/// Installs into `<root>/.mcp.json`; returns the path.
///
/// # Errors
/// The existing file is a link, unreadable or not a JSON object, or the write failed.
pub fn install_project(root: &Path, remote: Option<&str>) -> Result<PathBuf> {
    let path = root.join(".mcp.json");
    refuse_link(&path)?;
    let doc = match std::fs::read(&path) {
        Ok(b) => Some(serde_json::from_slice::<Value>(&b).map_err(|e| {
            fatal(format!(
                "{} is not valid JSON ({e}); not changing it",
                path.display()
            ))
        })?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(fatal(e)),
    };
    let merged = merge(doc, remote)?;
    let mut out = serde_json::to_vec_pretty(&merged).map_err(fatal)?;
    out.push(b'\n');
    write_atomic(&path, &out)?;
    Ok(path)
}

/// The `asc.mcp.*` defaults the user scope writes: the safe values, never wider ones.
const USER_KEYS: &[(&str, &str)] = &[
    ("write", "false"),
    ("toolsets", "core"),
    ("confirm", "destructive"),
];

/// The user's global config file: `GIT_CONFIG_GLOBAL`, else `$XDG_CONFIG_HOME/git/config` if it
/// exists, else `~/.gitconfig`.
fn user_config_path() -> Result<PathBuf> {
    let abs = |k: &str| {
        std::env::var_os(k)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    if let Some(p) = abs("GIT_CONFIG_GLOBAL") {
        return Ok(p);
    }
    let home = std::env::home_dir().filter(|h| h.is_absolute());
    let xdg = abs("XDG_CONFIG_HOME").or_else(|| home.as_ref().map(|h| h.join(".config")));
    if let Some(x) = xdg
        .map(|x| x.join("git").join("config"))
        .filter(|p| p.is_file())
    {
        return Ok(x);
    }
    home.map(|h| h.join(".gitconfig"))
        .ok_or_else(|| fatal("cannot find the user config (no home directory)"))
}

/// Appends the `[asc "mcp"]` keys that `existing` does not set; `None` when nothing is missing.
///
/// # Errors
/// `existing` does not parse as git config; it is then left untouched.
pub fn user_update(existing: &str) -> Result<Option<String>> {
    let file = gix_config::File::try_from(existing)
        .map_err(|e| fatal(format!("the user config does not parse ({e}); not changing it")))?;
    let missing: Vec<_> = USER_KEYS
        .iter()
        .filter(|(k, _)| file.raw_value_by("asc", Some("mcp".into()), *k).is_err())
        .collect();
    if missing.is_empty() {
        return Ok(None);
    }
    let mut out = existing.to_owned();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("[asc \"mcp\"]\n");
    for (k, v) in missing {
        out.push_str(&format!("\t{k} = {v}\n"));
    }
    Ok(Some(out))
}

/// Installs into the user's config; returns the path and whether anything changed.
///
/// # Errors
/// The config is a link, unreadable or malformed, or the write failed.
pub fn install_user() -> Result<(PathBuf, bool)> {
    let path = user_config_path()?;
    refuse_link(&path)?;
    let text = match std::fs::read(&path) {
        Ok(b) => String::from_utf8(b).map_err(|_| fatal("the user config is not UTF-8"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(fatal(e)),
    };
    match user_update(&text)? {
        Some(new) => {
            write_atomic(&path, new.as_bytes())?;
            Ok((path, true))
        }
        None => Ok((path, false)),
    }
}

/// `asc mcp install`.
///
/// # Errors
/// Usage errors, a missing workspace for project scope, and the write errors above.
pub fn run(ctx: &Ctx, argv: &[OsString]) -> Result<ExitCode> {
    let loopback = std::env::var_os("ASC_ALLOW_LOOPBACK_HTTP").is_some_and(|v| v == "1");
    let opts = parse(argv, loopback)?;
    match opts.scope {
        Scope::Project => {
            let root = ctx.ws()?.projection.toplevel().to_path_buf();
            let path = install_project(&root, opts.remote.as_deref())?;
            println!("asc: wrote the asc entry to {}", path.display());
        }
        Scope::User => {
            let (path, changed) = install_user()?;
            if changed {
                println!("asc: wrote asc.mcp.* defaults to {}", path.display());
            } else {
                println!("asc: asc.mcp.* already set in {}", path.display());
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn entry_is_read_only_and_narrowing() {
        let e = entry(None);
        assert_eq!(e["args"], json!(["mcp", "--read-only"]));
        let e = entry(Some("https://h.example/x"));
        assert_eq!(
            e["args"],
            json!(["mcp", "--remote", "https://h.example/x", "--read-only"])
        );
        assert!(!e.to_string().contains("--write"));
    }

    #[test]
    fn remote_url_rules() {
        assert!(parse(&a(&["--remote", "https://h.example"]), false).is_ok());
        for bad in [
            "http://h.example",
            "https://u:p@h.example",
            "https://tok@h.example",
            "https://h.example/?t=1",
            "https://h.example/#f",
            "ssh://h.example",
            "http://127.0.0.1:1",
        ] {
            assert!(parse(&a(&["--remote", bad]), false).is_err(), "{bad}");
        }
        assert!(parse(&a(&["--remote", "http://127.0.0.1:1"]), true).is_ok());
        assert!(parse(&a(&["--remote", "http://h.example"]), true).is_err());
        assert!(parse(&a(&["--scope", "user", "--remote", "https://h.example"]), false).is_err());
        assert!(parse(&a(&["--scope", "x"]), false).is_err());
    }

    #[test]
    fn merge_keeps_others_and_refuses_duplicate_endpoint() {
        let doc = json!({"mcpServers": {"other": {"command": "x"}}, "k": 1});
        let m = merge(Some(doc), None).unwrap();
        assert_eq!(m["mcpServers"]["other"]["command"], "x");
        assert_eq!(m["k"], 1);
        assert_eq!(m["mcpServers"]["asc"]["args"][1], "--read-only");
        let dup = json!({"mcpServers": {"hub": {"type": "http", "url": "https://h.example/mcp"}}});
        assert!(merge(Some(dup), Some("https://h.example")).is_err());
        assert!(merge(Some(json!([])), None).is_err());
    }

    #[test]
    fn user_update_adds_only_missing() {
        let out = user_update("[user]\n\tname = A\n").unwrap().unwrap();
        assert!(out.contains("[asc \"mcp\"]") && out.contains("write = false"));
        assert!(user_update(&out).unwrap().is_none());
        let keep = "[asc \"mcp\"]\n\twrite = remote\n";
        let out = user_update(keep).unwrap().unwrap();
        assert!(out.starts_with(keep) && !out.contains("write = false"));
    }
}
