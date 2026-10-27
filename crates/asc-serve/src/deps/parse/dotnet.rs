//! `packages.lock.json` and the MSBuild project files (`*.csproj`, `*.fsproj`, `*.vbproj`).

use std::collections::HashMap;

use super::Parsed;
use super::xml::{self, Node};
use crate::deps::model::{Dep, Ecosystem};
use crate::deps::version::{Scheme, Version, requirement_floor};

const ECO: Ecosystem = Ecosystem::NuGet;

pub(super) fn packages_lock(text: &str, p: &mut Parsed) -> Result<(), String> {
    let doc: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("not a packages.lock.json: {e}"))?;
    let frameworks = doc
        .get("dependencies")
        .and_then(serde_json::Value::as_object)
        .ok_or("not a packages.lock.json: no dependencies")?;
    let path = p.path.clone();
    let mut left_out = 0usize;
    for packages in frameworks.values().filter_map(serde_json::Value::as_object) {
        for (name, entry) in packages {
            let kind = entry.get("type").and_then(serde_json::Value::as_str);
            if kind == Some("Project") {
                continue;
            }
            let Some(v) = entry
                .get("resolved")
                .and_then(serde_json::Value::as_str)
                .filter(|v| Version::parse(Scheme::Semver, v).is_some())
            else {
                left_out += 1;
                continue;
            };
            let mut d = Dep::lock(ECO, name, v, &path);
            d.direct = kind == Some("Direct");
            p.pin(d)?;
        }
    }
    if left_out > 0 {
        p.note(format!(
            "{left_out} entries have no resolved version and are not checked"
        ));
    }
    Ok(())
}

fn references<'a>(n: &'a Node, out: &mut Vec<&'a Node>) {
    for c in &n.children {
        if c.name == "PackageReference" {
            out.push(c);
        } else {
            references(c, out);
        }
    }
}

/// Replaces `$(Name)` with a property of the file; `None` when a reference has no value.
fn resolve(s: &str, props: &HashMap<String, String>) -> Option<String> {
    let mut cur = s.trim().to_owned();
    for _ in 0..10 {
        let Some(start) = cur.find("$(") else {
            return Some(cur);
        };
        let end = cur[start..].find(')')? + start;
        let value = props.get(&cur[start + 2..end])?;
        cur = format!("{}{}{}", &cur[..start], value, &cur[end + 1..]);
    }
    cur.find("$(").is_none().then_some(cur)
}

pub(super) fn csproj(text: &str, p: &mut Parsed) -> Result<(), String> {
    let root = xml::parse(text).map_err(|e| format!("not an MSBuild project: {e}"))?;
    if root.name != "Project" {
        return Err("not an MSBuild project: the root element is not <Project>".into());
    }
    let path = p.path.clone();
    let mut props: HashMap<String, String> = HashMap::new();
    for group in root.named("PropertyGroup") {
        for c in &group.children {
            props.insert(c.name.clone(), c.text.trim().to_owned());
        }
    }
    let mut refs = Vec::new();
    references(&root, &mut refs);
    let mut left_out = 0usize;
    for r in refs {
        let Some(name) = r.attr("Include").map(str::trim).filter(|n| !n.is_empty()) else {
            continue;
        };
        let raw = r
            .attr("Version")
            .or_else(|| r.attr("VersionOverride"))
            .map(str::to_owned)
            .or_else(|| r.text_of("Version"))
            .or_else(|| r.text_of("VersionOverride"));
        let Some(raw) = raw else {
            left_out += 1;
            continue;
        };
        let Some(version) = resolve(&raw, &props) else {
            left_out += 1;
            continue;
        };
        if Version::parse(Scheme::Semver, &version).is_some() {
            let mut d = Dep::lock(ECO, name, &version, &path);
            d.direct = true;
            p.pin(d)?;
        } else if let Some(floor) = requirement_floor(&version) {
            p.push(Dep::manifest(ECO, name, &version, &floor, &path))?;
        } else {
            left_out += 1;
        }
    }
    if left_out > 0 {
        p.note(format!(
            "{left_out} package references have no version in this file (central management or a property) and are not checked"
        ));
    }
    Ok(())
}
