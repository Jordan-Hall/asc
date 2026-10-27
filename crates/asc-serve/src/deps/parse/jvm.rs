//! `pom.xml` and `gradle.lockfile`.

use std::collections::HashMap;

use super::Parsed;
use super::xml::{self, Node};
use crate::deps::model::{Dep, Ecosystem};
use crate::deps::version::{Scheme, Version, requirement_floor};

const ECO: Ecosystem = Ecosystem::Maven;

/// Replaces `${name}` with its value, repeatedly; `None` when a reference has no value.
fn resolve(s: &str, props: &HashMap<String, String>) -> Option<String> {
    let mut cur = s.trim().to_owned();
    for _ in 0..10 {
        let Some(start) = cur.find("${") else {
            return Some(cur);
        };
        let end = cur[start..].find('}')? + start;
        let key = &cur[start + 2..end];
        let value = props.get(key)?;
        cur = format!("{}{}{}", &cur[..start], value, &cur[end + 1..]);
    }
    cur.find("${").is_none().then_some(cur)
}

fn collect<'a>(node: &'a Node, in_management: bool, out: &mut Vec<(&'a Node, bool)>) {
    for c in &node.children {
        match c.name.as_str() {
            "dependencyManagement" => collect(c, true, out),
            "dependencies" => out.extend(c.named("dependency").map(|d| (d, in_management))),
            // Plugins may list dependencies of their own; so do profiles.
            "profiles" | "profile" | "build" | "plugins" | "plugin" | "pluginManagement" => {
                collect(c, in_management, out);
            }
            _ => {}
        }
    }
}

pub(super) fn pom(text: &str, p: &mut Parsed) -> Result<(), String> {
    let root = xml::parse(text).map_err(|e| format!("not a pom.xml: {e}"))?;
    if root.name != "project" {
        return Err("not a pom.xml: the root element is not <project>".into());
    }
    let path = p.path.clone();
    let mut props: HashMap<String, String> = HashMap::new();
    if let Some(ps) = root.child("properties") {
        for c in &ps.children {
            props.insert(c.name.clone(), c.text.trim().to_owned());
        }
    }
    let parent = root.child("parent");
    let pick = |name: &str| {
        root.text_of(name)
            .or_else(|| parent.and_then(|p| p.text_of(name)))
    };
    if let Some(v) = pick("version") {
        for k in ["project.version", "pom.version", "version"] {
            props.entry(k.to_owned()).or_insert_with(|| v.clone());
        }
    }
    if let Some(g) = pick("groupId") {
        for k in ["project.groupId", "pom.groupId", "groupId"] {
            props.entry(k.to_owned()).or_insert_with(|| g.clone());
        }
    }
    if let Some(a) = root.text_of("artifactId") {
        props.entry("project.artifactId".to_owned()).or_insert(a);
    }
    if let Some(v) = parent.and_then(|p| p.text_of("version")) {
        props
            .entry("project.parent.version".to_owned())
            .or_insert(v);
    }
    let mut found = Vec::new();
    collect(&root, false, &mut found);
    let mut managed: HashMap<(String, String), String> = HashMap::new();
    for (d, mgmt) in &found {
        if !*mgmt {
            continue;
        }
        let (Some(g), Some(a), Some(v)) = (
            d.text_of("groupId").and_then(|g| resolve(&g, &props)),
            d.text_of("artifactId").and_then(|a| resolve(&a, &props)),
            d.text_of("version").and_then(|v| resolve(&v, &props)),
        ) else {
            continue;
        };
        managed.insert((g, a), v);
    }
    let mut left_out = 0usize;
    for (d, mgmt) in &found {
        if *mgmt {
            continue;
        }
        if d.text_of("scope").as_deref() == Some("import") {
            continue;
        }
        let (Some(g), Some(a)) = (
            d.text_of("groupId").and_then(|g| resolve(&g, &props)),
            d.text_of("artifactId").and_then(|a| resolve(&a, &props)),
        ) else {
            left_out += 1;
            continue;
        };
        let declared = d.text_of("version");
        let version = match declared {
            Some(v) => resolve(&v, &props),
            None => managed.get(&(g.clone(), a.clone())).cloned(),
        };
        let Some(version) = version else {
            left_out += 1;
            continue;
        };
        let name = format!("{g}:{a}");
        if version.starts_with(['[', '(']) {
            match requirement_floor(&version) {
                Some(floor) => p.push(Dep::manifest(ECO, &name, &version, &floor, &path))?,
                None => left_out += 1,
            }
        } else if Version::parse(Scheme::Maven, &version).is_some() {
            let mut dep = Dep::lock(ECO, &name, &version, &path);
            dep.direct = true;
            p.pin(dep)?;
        } else {
            left_out += 1;
        }
    }
    if left_out > 0 {
        p.note(format!(
            "{left_out} dependencies have a version this file does not state (a parent or a BOM) and are not checked"
        ));
    }
    Ok(())
}

pub(super) fn gradle_lock(text: &str, p: &mut Parsed) -> Result<(), String> {
    let path = p.path.clone();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("empty=") {
            continue;
        }
        let coords = line.split_once('=').map_or(line, |(c, _)| c);
        let parts: Vec<&str> = coords.split(':').collect();
        let [group, artifact, version] = parts[..] else {
            return Err("not a gradle.lockfile: a line is not group:artifact:version".into());
        };
        if Version::parse(Scheme::Maven, version).is_some() {
            p.pin(Dep::lock(
                ECO,
                &format!("{group}:{artifact}"),
                version,
                &path,
            ))?;
        }
    }
    Ok(())
}
