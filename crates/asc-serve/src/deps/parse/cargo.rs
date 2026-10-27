//! `Cargo.lock` and `Cargo.toml`.

use std::collections::BTreeSet;

use serde::Deserialize;

use super::Parsed;
use crate::deps::model::{Dep, Ecosystem};
use crate::deps::version::requirement_floor;

const ECO: Ecosystem = Ecosystem::CratesIo;

#[derive(Deserialize)]
struct Lock {
    #[serde(default)]
    package: Vec<Pkg>,
}

#[derive(Deserialize)]
struct Pkg {
    name: String,
    version: String,
    source: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

/// Whether a lock `source` is the crates.io registry, in either index form.
fn is_crates_io(source: &str) -> bool {
    source == "registry+https://github.com/rust-lang/crates.io-index"
        || source == "sparse+https://index.crates.io/"
}

/// A package of the lockfile with a source is a dependency; one without is a member of the workspace,
/// and what it depends on is what the repository names itself.
pub(super) fn lock(text: &str, p: &mut Parsed) -> Result<(), String> {
    let lock: Lock = toml::from_str(text).map_err(|e| format!("not a Cargo.lock: {e}"))?;
    // Entries of a `dependencies` list are `name`, `name version` or `name version (source)`.
    let mut direct: BTreeSet<(String, Option<String>)> = BTreeSet::new();
    for m in lock.package.iter().filter(|m| m.source.is_none()) {
        for d in &m.dependencies {
            let mut it = d.split(' ');
            let name = it.next().unwrap_or_default().to_owned();
            direct.insert((name, it.next().map(str::to_owned)));
        }
    }
    let path = p.path.clone();
    let mut other = 0usize;
    for pkg in &lock.package {
        let Some(source) = &pkg.source else { continue };
        if !is_crates_io(source) {
            other += 1;
            continue;
        }
        let mut d = Dep::lock(ECO, &pkg.name, &pkg.version, &path);
        d.direct = direct.contains(&(pkg.name.clone(), None))
            || direct.contains(&(pkg.name.clone(), Some(pkg.version.clone())));
        p.pin(d)?;
    }
    if other > 0 {
        p.note(format!(
            "{other} packages come from git or another registry and are not checked"
        ));
    }
    Ok(())
}

/// The tables of a manifest that hold dependencies.
const TABLES: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

pub(super) fn manifest(text: &str, p: &mut Parsed) -> Result<(), String> {
    let doc: toml::Table = toml::from_str(text).map_err(|e| format!("not a Cargo.toml: {e}"))?;
    let mut tables: Vec<&toml::Table> = Vec::new();
    for t in TABLES {
        if let Some(toml::Value::Table(x)) = doc.get(t) {
            tables.push(x);
        }
    }
    if let Some(toml::Value::Table(w)) = doc.get("workspace")
        && let Some(toml::Value::Table(x)) = w.get("dependencies")
    {
        tables.push(x);
    }
    if let Some(toml::Value::Table(targets)) = doc.get("target") {
        for cfg in targets.values() {
            if let toml::Value::Table(cfg) = cfg {
                for t in TABLES {
                    if let Some(toml::Value::Table(x)) = cfg.get(t) {
                        tables.push(x);
                    }
                }
            }
        }
    }
    let path = p.path.clone();
    let mut skipped = 0usize;
    for table in tables {
        for (key, spec) in table {
            let (name, req) = match spec {
                toml::Value::String(r) => (key.as_str(), r.as_str()),
                toml::Value::Table(t) => {
                    if t.contains_key("path")
                        || t.contains_key("git")
                        || t.contains_key("registry")
                        || t.get("workspace") == Some(&toml::Value::Boolean(true))
                    {
                        skipped += 1;
                        continue;
                    }
                    let Some(toml::Value::String(r)) = t.get("version") else {
                        skipped += 1;
                        continue;
                    };
                    let name = match t.get("package") {
                        Some(toml::Value::String(n)) => n.as_str(),
                        _ => key.as_str(),
                    };
                    (name, r.as_str())
                }
                _ => continue,
            };
            let Some(floor) = requirement_floor(req) else {
                skipped += 1;
                continue;
            };
            p.push(Dep::manifest(ECO, name, req, &floor, &path))?;
        }
    }
    if skipped > 0 {
        p.note(format!(
            "{skipped} dependencies are path, git, workspace or unversioned entries and are not checked"
        ));
    }
    Ok(())
}
