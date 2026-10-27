//! `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, `pnpm-lock.yaml` and `package.json`.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde::de::IgnoredAny;

use super::Parsed;
use crate::deps::model::{Dep, Ecosystem};
use crate::deps::version::{Scheme, Version, requirement_floor};
use crate::deps::yaml::{self, Y};

const ECO: Ecosystem = Ecosystem::Npm;

fn is_version(v: &str) -> bool {
    Version::parse(Scheme::Semver, v).is_some()
}

// ------------------------------------------------------------------------------ package-lock.json

#[derive(Deserialize)]
struct Lock {
    #[serde(default, rename = "lockfileVersion")]
    lockfile_version: Option<u32>,
    #[serde(default)]
    packages: BTreeMap<String, Pkg>,
    #[serde(default)]
    dependencies: BTreeMap<String, V1>,
}

#[derive(Deserialize, Default)]
struct Pkg {
    name: Option<String>,
    version: Option<String>,
    #[serde(default)]
    link: bool,
    #[serde(default)]
    dependencies: BTreeMap<String, IgnoredAny>,
    #[serde(default, rename = "devDependencies")]
    dev_dependencies: BTreeMap<String, IgnoredAny>,
    #[serde(default, rename = "optionalDependencies")]
    optional_dependencies: BTreeMap<String, IgnoredAny>,
}

#[derive(Deserialize)]
struct V1 {
    version: Option<String>,
    #[serde(default)]
    dependencies: BTreeMap<String, V1>,
}

pub(super) fn package_lock(text: &str, p: &mut Parsed) -> Result<(), String> {
    let lock: Lock =
        serde_json::from_str(text).map_err(|e| format!("not a package-lock.json: {e}"))?;
    if lock.lockfile_version.is_none() && lock.packages.is_empty() && lock.dependencies.is_empty() {
        return Err("not a package-lock.json: no lockfileVersion and no packages".into());
    }
    let path = p.path.clone();
    let mut left_out = 0usize;
    if !lock.packages.is_empty() {
        // lockfileVersion 2 and 3: the root and the workspace members name what is direct.
        let mut direct: BTreeSet<&str> = BTreeSet::new();
        for (key, pkg) in &lock.packages {
            if key.is_empty() || !key.contains("node_modules/") {
                for n in pkg
                    .dependencies
                    .keys()
                    .chain(pkg.dev_dependencies.keys())
                    .chain(pkg.optional_dependencies.keys())
                {
                    direct.insert(n);
                }
            }
        }
        for (key, pkg) in &lock.packages {
            let Some(at) = key.rfind("node_modules/") else {
                continue;
            };
            if pkg.link {
                continue;
            }
            let name = pkg
                .name
                .as_deref()
                .unwrap_or(&key[at + "node_modules/".len()..]);
            let Some(version) = pkg.version.as_deref().filter(|v| is_version(v)) else {
                left_out += 1;
                continue;
            };
            let mut d = Dep::lock(ECO, name, version, &path);
            d.direct = key == &format!("node_modules/{name}") && direct.contains(name);
            p.pin(d)?;
        }
    } else {
        // lockfileVersion 1: a tree of nested `dependencies`. The top level holds hoisted packages
        // too, so which are direct is learned from the package.json beside the lockfile.
        fn walk(
            deps: &BTreeMap<String, V1>,
            depth: usize,
            p: &mut Parsed,
            path: &str,
            left_out: &mut usize,
        ) -> Result<(), String> {
            if depth > 64 {
                return Err("the dependency tree is nested too deeply".into());
            }
            for (name, d) in deps {
                match d.version.as_deref().filter(|v| is_version(v)) {
                    Some(v) => p.pin(Dep::lock(ECO, name, v, path))?,
                    None => *left_out += 1,
                }
                walk(&d.dependencies, depth + 1, p, path, left_out)?;
            }
            Ok(())
        }
        walk(&lock.dependencies, 0, p, &path, &mut left_out)?;
    }
    if left_out > 0 {
        p.note(format!(
            "{left_out} packages have no registry version (git, file or link) and are not checked"
        ));
    }
    Ok(())
}

// ------------------------------------------------------------------------------ yarn.lock

fn unquote(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .unwrap_or(s)
}

#[derive(Default)]
struct YarnEntry {
    names: Vec<String>,
    version: Option<String>,
    resolution: Option<String>,
    resolved: Option<String>,
}

/// `name` of a `name@range` spec: everything before the last `@` that is not the first character.
fn spec_name(spec: &str) -> Option<&str> {
    let i = spec.rfind('@')?;
    (i > 0).then(|| &spec[..i])
}

fn yarn_flush(
    e: Option<YarnEntry>,
    p: &mut Parsed,
    path: &str,
    left_out: &mut usize,
) -> Result<(), String> {
    let Some(e) = e else { return Ok(()) };
    let Some(first) = e.names.first() else {
        return Ok(());
    };
    let Some(version) = e.version.as_deref() else {
        return Ok(());
    };
    let mut name = first.as_str();
    if let Some(res) = &e.resolution {
        // Yarn berry: `name@npm:1.2.3` is a registry package; `name@workspace:.`, `patch:`, `git` are not.
        match res.rfind("@npm:") {
            Some(i) if i > 0 => name = &res[..i],
            _ => {
                if !res.contains("@workspace:") {
                    *left_out += 1;
                }
                return Ok(());
            }
        }
    } else if let Some(r) = &e.resolved
        && (r.starts_with("git") || r.starts_with("file:") || r.contains("codeload.github.com"))
    {
        *left_out += 1;
        return Ok(());
    }
    // An aliased spec (`name@npm:other@^1`) names a package the header does not.
    if !is_version(version) || name.get(1..).is_some_and(|r| r.contains('@')) {
        *left_out += 1;
        return Ok(());
    }
    p.pin(Dep::lock(ECO, name, version, path))
}

pub(super) fn yarn_lock(text: &str, p: &mut Parsed) -> Result<(), String> {
    let path = p.path.clone();
    let mut left_out = 0usize;
    let mut cur: Option<YarnEntry> = None;
    let mut seen_entry = false;
    for line in text.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        if !line.starts_with(' ') {
            yarn_flush(cur.take(), p, &path, &mut left_out)?;
            let Some(head) = line.strip_suffix(':') else {
                return Err("not a yarn.lock: a line outside an entry".into());
            };
            if head == "__metadata" {
                continue;
            }
            seen_entry = true;
            let mut e = YarnEntry::default();
            for spec in head.split(',') {
                if let Some(n) = spec_name(unquote(spec)) {
                    let n = n.to_owned();
                    if !e.names.contains(&n) {
                        e.names.push(n);
                    }
                }
            }
            cur = Some(e);
            continue;
        }
        let Some(e) = cur.as_mut() else { continue };
        let Some(field) = line.strip_prefix("  ").filter(|f| !f.starts_with(' ')) else {
            continue;
        };
        let value = |key: &str| -> Option<String> {
            field
                .strip_prefix(key)
                .and_then(|r| r.strip_prefix(':').or(Some(r)))
                .filter(|r| r.starts_with(' '))
                .map(|r| unquote(r).to_owned())
        };
        if let Some(v) = value("version") {
            e.version = Some(v);
        } else if let Some(v) = value("resolution") {
            e.resolution = Some(v);
        } else if let Some(v) = value("resolved") {
            e.resolved = Some(v);
        }
    }
    yarn_flush(cur.take(), p, &path, &mut left_out)?;
    if !seen_entry && !text.trim().is_empty() && !text.contains("__metadata") {
        return Err("not a yarn.lock: no entries".into());
    }
    if left_out > 0 {
        p.note(format!(
            "{left_out} entries are workspace, git, file or patched packages and are not checked"
        ));
    }
    Ok(())
}

// ------------------------------------------------------------------------------ pnpm-lock.yaml

/// Splits a key of the `packages:` map into name and version: `/name/1.2.3` (lockfile 5),
/// `/name@1.2.3` (5.4 and 6) and `name@1.2.3` (9), with or without a peer suffix.
fn pnpm_key(key: &str) -> Option<(String, String)> {
    let mut k = key.strip_prefix('/').unwrap_or(key);
    if let Some(i) = k.find('(') {
        k = &k[..i];
    }
    let name_end = if k.starts_with('@') {
        k.match_indices('/').nth(1).map(|(i, _)| i)
    } else {
        k.find('/')
    };
    if let Some(i) = name_end
        && k[i + 1..].starts_with(|c: char| c.is_ascii_digit())
    {
        let ver = k[i + 1..].split('_').next()?;
        return Some((k[..i].to_owned(), ver.to_owned()));
    }
    let at = k.get(1..)?.rfind('@')? + 1;
    Some((k[..at].to_owned(), k[at + 1..].to_owned()))
}

fn pnpm_names(y: &Y, into: &mut BTreeSet<String>) {
    for section in ["dependencies", "devDependencies", "optionalDependencies"] {
        if let Some(m) = y.get(section) {
            for (name, _) in m.entries() {
                into.insert(name.clone());
            }
        }
    }
}

pub(super) fn pnpm_lock(text: &str, p: &mut Parsed) -> Result<(), String> {
    let doc = yaml::parse(text)?;
    if !matches!(doc, Y::Map(_)) {
        return Err("not a pnpm-lock.yaml".into());
    }
    let path = p.path.clone();
    let mut direct: BTreeSet<String> = BTreeSet::new();
    pnpm_names(&doc, &mut direct);
    if let Some(importers) = doc.get("importers") {
        for (_, imp) in importers.entries() {
            pnpm_names(imp, &mut direct);
        }
    }
    let Some(packages) = doc.get("packages") else {
        return Ok(());
    };
    let mut left_out = 0usize;
    for (key, val) in packages.entries() {
        let Some((name, version)) = pnpm_key(key) else {
            left_out += 1;
            continue;
        };
        // A registry package has an integrity; a tarball URL, a git repository or a directory has not.
        let registry = val
            .get("resolution")
            .is_some_and(|r| r.get("integrity").is_some());
        if !registry || !is_version(&version) {
            left_out += 1;
            continue;
        }
        let mut d = Dep::lock(ECO, &name, &version, &path);
        d.direct = direct.contains(&name);
        p.pin(d)?;
    }
    if left_out > 0 {
        p.note(format!(
            "{left_out} packages are git, tarball or directory sources and are not checked"
        ));
    }
    Ok(())
}

// ------------------------------------------------------------------------------ package.json

pub(super) fn package_json(text: &str, p: &mut Parsed) -> Result<(), String> {
    let doc: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("not a package.json: {e}"))?;
    let path = p.path.clone();
    let mut skipped = 0usize;
    for section in ["dependencies", "devDependencies", "optionalDependencies"] {
        let Some(map) = doc.get(section).and_then(serde_json::Value::as_object) else {
            continue;
        };
        for (name, req) in map {
            let Some(req) = req.as_str() else {
                skipped += 1;
                continue;
            };
            match requirement_floor(req) {
                Some(floor) => p.push(Dep::manifest(ECO, name, req, &floor, &path))?,
                None => skipped += 1,
            }
        }
    }
    if skipped > 0 {
        p.note(format!(
            "{skipped} dependencies are tags, aliases, workspace, git or file references and are not checked"
        ));
    }
    Ok(())
}
