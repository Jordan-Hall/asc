//! `requirements*.txt`, `poetry.lock`, `uv.lock`, `Pipfile.lock`, `pyproject.toml` and `Pipfile`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;

use super::Parsed;
use crate::deps::model::{Dep, Ecosystem};
use crate::deps::version::{Scheme, Version, requirement_floor};

const ECO: Ecosystem = Ecosystem::PyPi;

/// `name[extras] spec`: a PEP 508 requirement without its marker.
static REQ: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?)\s*(?:\[[^\]]*\])?\s*(.*)$")
        .expect("a literal pattern")
});

/// A PEP 508 requirement split into the name and the version specifier.
pub(super) fn split_requirement(s: &str) -> Option<(String, String)> {
    let s = s.split(';').next().unwrap_or(s).trim();
    let c = REQ.captures(s)?;
    let name = c.get(1)?.as_str().to_owned();
    let spec = c.get(2)?.as_str().trim();
    let spec = spec
        .strip_prefix('(')
        .and_then(|r| r.strip_suffix(')'))
        .unwrap_or(spec)
        .trim();
    // A direct reference (`name @ https://...`) names no version.
    if spec.starts_with('@') {
        return Some((name, "@".into()));
    }
    Some((name, spec.to_owned()))
}

/// The version of an exact pin (`==1.2.3`, `===1.2.3`) inside `spec`, when there is one and it has no
/// wildcard.
fn exact(spec: &str) -> Option<String> {
    spec.split(',').find_map(|c| {
        let c = c.trim();
        let v = c
            .strip_prefix("===")
            .or_else(|| c.strip_prefix("=="))?
            .trim();
        (!v.contains('*') && Version::parse(Scheme::Pep440, v).is_some()).then(|| v.to_owned())
    })
}

pub(super) fn requirements(text: &str, p: &mut Parsed) -> Result<(), String> {
    let path = p.path.clone();
    let has_via = text.contains("# via");
    // Joined physical lines, each with the `# via` items that follow it (pip-compile output).
    struct Item {
        line: String,
        via: Vec<String>,
    }
    let mut items: Vec<Item> = Vec::new();
    let mut joined = String::new();
    let mut in_via = false;
    for raw in text.lines() {
        let trimmed = raw.trim();
        if let Some(comment) = trimmed.strip_prefix('#') {
            let c = comment.trim();
            if let Some(rest) = c
                .strip_prefix("via")
                .filter(|r| r.is_empty() || r.starts_with(char::is_whitespace))
            {
                in_via = true;
                let rest = rest.trim();
                if !rest.is_empty()
                    && let Some(last) = items.last_mut()
                {
                    last.via.push(rest.to_owned());
                }
            } else if in_via && let Some(last) = items.last_mut() {
                last.via.push(c.to_owned());
            }
            continue;
        }
        in_via = false;
        let mut line = raw.to_owned();
        // A comment after a requirement starts at ` #`.
        if let Some(i) = line.find(" #") {
            line.truncate(i);
        }
        let cont = line.trim_end().ends_with('\\');
        let part = line.trim_end().trim_end_matches('\\');
        joined.push_str(part);
        joined.push(' ');
        if cont {
            continue;
        }
        let l = std::mem::take(&mut joined);
        if !l.trim().is_empty() {
            items.push(Item {
                line: l,
                via: Vec::new(),
            });
        }
    }
    let mut options = 0usize;
    let mut skipped = 0usize;
    for it in items {
        let line = it.line.trim();
        if line.starts_with('-') {
            options += 1;
            continue;
        }
        // Hash options trail the requirement.
        let line = line.split(" --").next().unwrap_or(line).trim();
        let Some((name, spec)) = split_requirement(line) else {
            skipped += 1;
            continue;
        };
        if spec == "@" {
            skipped += 1;
            continue;
        }
        let direct = !has_via
            || it
                .via
                .iter()
                .any(|v| v.starts_with("-r ") || v.starts_with("-c ") || v.starts_with("-r\t"));
        if let Some(v) = exact(&spec) {
            let mut d = Dep::lock(ECO, &name, &v, &path);
            d.direct = direct;
            p.pin(d)?;
        } else if let Some(floor) = requirement_floor(&spec) {
            let mut d = Dep::manifest(ECO, &name, &spec, &floor, &path);
            d.direct = direct;
            p.push(d)?;
        } else {
            skipped += 1;
        }
    }
    if skipped > 0 {
        p.note(format!(
            "{skipped} requirements have no version or are direct URLs and are not checked"
        ));
    }
    if options > 0 {
        p.note(format!(
            "{options} option lines (-r, -c, -e, index URLs) are not followed"
        ));
    }
    Ok(())
}

// ------------------------------------------------------------------------------ poetry.lock

#[derive(Deserialize)]
struct PLock {
    #[serde(default)]
    package: Vec<PPkg>,
}

#[derive(Deserialize)]
struct PPkg {
    name: String,
    version: String,
    source: Option<toml::Table>,
}

pub(super) fn poetry_lock(text: &str, p: &mut Parsed) -> Result<(), String> {
    let lock: PLock = toml::from_str(text).map_err(|e| format!("not a poetry.lock: {e}"))?;
    let path = p.path.clone();
    let mut other = 0usize;
    for pkg in lock.package {
        let local = pkg
            .source
            .as_ref()
            .and_then(|s| s.get("type"))
            .and_then(toml::Value::as_str)
            .is_some_and(|t| matches!(t, "git" | "directory" | "file" | "url"));
        if local || Version::parse(Scheme::Pep440, &pkg.version).is_none() {
            other += 1;
            continue;
        }
        p.pin(Dep::lock(ECO, &pkg.name, &pkg.version, &path))?;
    }
    if other > 0 {
        p.note(format!(
            "{other} packages are git, path or URL sources and are not checked"
        ));
    }
    Ok(())
}

// ------------------------------------------------------------------------------ uv.lock

#[derive(Deserialize)]
struct UvLock {
    #[serde(default)]
    package: Vec<UvPkg>,
}

#[derive(Deserialize)]
struct UvName {
    name: String,
}

#[derive(Deserialize)]
struct UvPkg {
    name: String,
    version: Option<String>,
    source: Option<toml::Table>,
    #[serde(default)]
    dependencies: Vec<UvName>,
    #[serde(default, rename = "dev-dependencies")]
    dev_dependencies: BTreeMap<String, Vec<UvName>>,
    #[serde(default, rename = "optional-dependencies")]
    optional_dependencies: BTreeMap<String, Vec<UvName>>,
}

pub(super) fn uv_lock(text: &str, p: &mut Parsed) -> Result<(), String> {
    let lock: UvLock = toml::from_str(text).map_err(|e| format!("not a uv.lock: {e}"))?;
    let path = p.path.clone();
    let mut direct: BTreeSet<String> = BTreeSet::new();
    for pkg in &lock.package {
        let member = pkg
            .source
            .as_ref()
            .is_some_and(|s| s.contains_key("virtual") || s.contains_key("editable"));
        if member {
            for d in pkg
                .dependencies
                .iter()
                .chain(pkg.dev_dependencies.values().flatten())
                .chain(pkg.optional_dependencies.values().flatten())
            {
                direct.insert(ECO.canonical(&d.name));
            }
        }
    }
    let mut other = 0usize;
    for pkg in &lock.package {
        let registry = pkg
            .source
            .as_ref()
            .is_some_and(|s| s.contains_key("registry"));
        let member = pkg
            .source
            .as_ref()
            .is_some_and(|s| s.contains_key("virtual") || s.contains_key("editable"));
        if member {
            continue;
        }
        let Some(v) = pkg.version.as_deref().filter(|_| registry) else {
            other += 1;
            continue;
        };
        let mut d = Dep::lock(ECO, &pkg.name, v, &path);
        d.direct = direct.contains(&ECO.canonical(&pkg.name));
        p.pin(d)?;
    }
    if other > 0 {
        p.note(format!(
            "{other} packages are git, path or URL sources and are not checked"
        ));
    }
    Ok(())
}

// ------------------------------------------------------------------------------ Pipfile.lock

pub(super) fn pipfile_lock(text: &str, p: &mut Parsed) -> Result<(), String> {
    let doc: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("not a Pipfile.lock: {e}"))?;
    let path = p.path.clone();
    let mut other = 0usize;
    for section in ["default", "develop"] {
        let Some(map) = doc.get(section).and_then(serde_json::Value::as_object) else {
            continue;
        };
        for (name, entry) in map {
            let version = entry
                .get("version")
                .and_then(serde_json::Value::as_str)
                .and_then(|v| v.strip_prefix("==="))
                .or_else(|| {
                    entry
                        .get("version")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|v| v.strip_prefix("=="))
                });
            let local = ["git", "path", "file", "editable", "ref"]
                .iter()
                .any(|k| entry.get(*k).is_some());
            match version {
                Some(v) if !local && Version::parse(Scheme::Pep440, v).is_some() => {
                    p.pin(Dep::lock(ECO, name, v, &path))?;
                }
                _ => other += 1,
            }
        }
    }
    if other > 0 {
        p.note(format!(
            "{other} packages are git, path or unpinned entries and are not checked"
        ));
    }
    Ok(())
}

// ------------------------------------------------------------------------------ pyproject.toml

fn add_requirement(p: &mut Parsed, path: &str, s: &str, skipped: &mut usize) -> Result<(), String> {
    let Some((name, spec)) = split_requirement(s) else {
        *skipped += 1;
        return Ok(());
    };
    match requirement_floor(&spec) {
        Some(floor) => p.push(Dep::manifest(ECO, &name, &spec, &floor, path)),
        None => {
            // A name with no version still says the package is direct.
            if spec != "@" {
                p.push(Dep::manifest(ECO, &name, "", "", path))?;
            }
            *skipped += 1;
            Ok(())
        }
    }
}

fn poetry_group(
    t: &toml::Table,
    p: &mut Parsed,
    path: &str,
    skipped: &mut usize,
) -> Result<(), String> {
    for (name, spec) in t {
        if name == "python" {
            continue;
        }
        let req = match spec {
            toml::Value::String(s) => s.clone(),
            toml::Value::Table(t) => {
                if ["git", "path", "url", "source"]
                    .iter()
                    .any(|k| t.contains_key(*k))
                {
                    *skipped += 1;
                    continue;
                }
                match t.get("version") {
                    Some(toml::Value::String(s)) => s.clone(),
                    _ => {
                        *skipped += 1;
                        continue;
                    }
                }
            }
            _ => {
                *skipped += 1;
                continue;
            }
        };
        match requirement_floor(&req) {
            Some(floor) => p.push(Dep::manifest(ECO, name, &req, &floor, path))?,
            None => {
                p.push(Dep::manifest(ECO, name, "", "", path))?;
                *skipped += 1;
            }
        }
    }
    Ok(())
}

pub(super) fn pyproject(text: &str, p: &mut Parsed) -> Result<(), String> {
    let doc: toml::Table =
        toml::from_str(text).map_err(|e| format!("not a pyproject.toml: {e}"))?;
    let path = p.path.clone();
    let mut skipped = 0usize;
    if let Some(toml::Value::Table(project)) = doc.get("project") {
        if let Some(toml::Value::Array(a)) = project.get("dependencies") {
            for s in a.iter().filter_map(toml::Value::as_str) {
                add_requirement(p, &path, s, &mut skipped)?;
            }
        }
        if let Some(toml::Value::Table(o)) = project.get("optional-dependencies") {
            for a in o.values().filter_map(toml::Value::as_array) {
                for s in a.iter().filter_map(toml::Value::as_str) {
                    add_requirement(p, &path, s, &mut skipped)?;
                }
            }
        }
    }
    if let Some(toml::Value::Table(g)) = doc.get("dependency-groups") {
        for a in g.values().filter_map(toml::Value::as_array) {
            for s in a.iter().filter_map(toml::Value::as_str) {
                add_requirement(p, &path, s, &mut skipped)?;
            }
        }
    }
    if let Some(toml::Value::Table(tool)) = doc.get("tool")
        && let Some(toml::Value::Table(poetry)) = tool.get("poetry")
    {
        for key in ["dependencies", "dev-dependencies"] {
            if let Some(toml::Value::Table(t)) = poetry.get(key) {
                poetry_group(t, p, &path, &mut skipped)?;
            }
        }
        if let Some(toml::Value::Table(groups)) = poetry.get("group") {
            for g in groups.values() {
                if let Some(toml::Value::Table(t)) = g.get("dependencies") {
                    poetry_group(t, p, &path, &mut skipped)?;
                }
            }
        }
    }
    if skipped > 0 {
        p.note(format!(
            "{skipped} requirements have no usable version and are not checked"
        ));
    }
    Ok(())
}

pub(super) fn pipfile(text: &str, p: &mut Parsed) -> Result<(), String> {
    let doc: toml::Table = toml::from_str(text).map_err(|e| format!("not a Pipfile: {e}"))?;
    let path = p.path.clone();
    let mut skipped = 0usize;
    for section in ["packages", "dev-packages"] {
        if let Some(toml::Value::Table(t)) = doc.get(section) {
            poetry_group(t, p, &path, &mut skipped)?;
        }
    }
    if skipped > 0 {
        p.note(format!(
            "{skipped} entries have no usable version and are not checked"
        ));
    }
    Ok(())
}
