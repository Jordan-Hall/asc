//! `Package.resolved` and `Package.swift`.

use std::sync::LazyLock;

use regex::Regex;

use super::Parsed;
use crate::deps::model::{Dep, Ecosystem};
use crate::deps::version::{Scheme, Version};

const ECO: Ecosystem = Ecosystem::SwiftUrl;

static PACKAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\.package\(\s*(?:name:\s*"[^"]*",\s*)?url:\s*"([^"]+)""#)
        .expect("a literal pattern")
});

/// The name OSV gives a Swift package: the repository URL without scheme, user and `.git`
/// (`github.com/apple/swift-nio`).
pub fn url_name(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = if let Some((_, r)) = url.split_once("://") {
        r.to_owned()
    } else if let Some((_, r)) = url.split_once('@') {
        // scp-like `git@github.com:apple/swift-nio.git`
        r.replacen(':', "/", 1)
    } else {
        return None;
    };
    let (authority, path) = rest.split_once('/')?;
    // Neither a user name nor a port is part of the name.
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    (!host.is_empty() && !path.is_empty() && !path.contains("..")).then(|| format!("{host}/{path}"))
}

pub(super) fn package_resolved(text: &str, p: &mut Parsed) -> Result<(), String> {
    let doc: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("not a Package.resolved: {e}"))?;
    let pins = doc
        .get("pins")
        .or_else(|| doc.get("object").and_then(|o| o.get("pins")))
        .and_then(serde_json::Value::as_array)
        .ok_or("not a Package.resolved: no pins")?;
    let path = p.path.clone();
    let mut left_out = 0usize;
    for pin in pins {
        let kind = pin.get("kind").and_then(serde_json::Value::as_str);
        if matches!(kind, Some("localSourceControl" | "fileSystem" | "registry")) {
            left_out += 1;
            continue;
        }
        let location = pin
            .get("location")
            .or_else(|| pin.get("repositoryURL"))
            .and_then(serde_json::Value::as_str);
        let version = pin
            .get("state")
            .and_then(|s| s.get("version"))
            .and_then(serde_json::Value::as_str)
            .filter(|v| Version::parse(Scheme::Semver, v).is_some());
        let (Some(name), Some(version)) = (location.and_then(url_name), version) else {
            left_out += 1;
            continue;
        };
        p.pin(Dep::lock(ECO, &name, version, &path))?;
    }
    if left_out > 0 {
        p.note(format!(
            "{left_out} pins are branches, revisions, local or registry sources and are not checked"
        ));
    }
    Ok(())
}

/// The packages a `Package.swift` names, for what is direct. Swift is not run.
pub(super) fn package_swift(text: &str, p: &mut Parsed) -> Result<(), String> {
    let path = p.path.clone();
    for c in PACKAGE.captures_iter(text) {
        if let Some(name) = url_name(&c[1]) {
            p.push(Dep::manifest(ECO, &name, "", "", &path))?;
        }
    }
    Ok(())
}
