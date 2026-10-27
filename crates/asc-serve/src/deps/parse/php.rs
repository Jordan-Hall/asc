//! `composer.lock` and `composer.json`.

use super::Parsed;
use crate::deps::model::{Dep, Ecosystem};
use crate::deps::version::{Scheme, Version, requirement_floor};

const ECO: Ecosystem = Ecosystem::Packagist;

pub(super) fn composer_lock(text: &str, p: &mut Parsed) -> Result<(), String> {
    let doc: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("not a composer.lock: {e}"))?;
    if !doc.is_object() || (doc.get("packages").is_none() && doc.get("packages-dev").is_none()) {
        return Err("not a composer.lock: no packages".into());
    }
    let path = p.path.clone();
    let mut left_out = 0usize;
    for section in ["packages", "packages-dev"] {
        let Some(list) = doc.get(section).and_then(serde_json::Value::as_array) else {
            continue;
        };
        for pkg in list {
            let name = pkg.get("name").and_then(serde_json::Value::as_str);
            let version = pkg.get("version").and_then(serde_json::Value::as_str);
            let (Some(name), Some(version)) = (name, version) else {
                left_out += 1;
                continue;
            };
            let version = version.strip_prefix('v').unwrap_or(version);
            if version.starts_with("dev-")
                || version.ends_with("-dev")
                || Version::parse(Scheme::Composer, version).is_none()
            {
                left_out += 1;
                continue;
            }
            p.pin(Dep::lock(ECO, name, version, &path))?;
        }
    }
    if left_out > 0 {
        p.note(format!(
            "{left_out} packages are development branches and are not checked"
        ));
    }
    Ok(())
}

pub(super) fn composer_json(text: &str, p: &mut Parsed) -> Result<(), String> {
    let doc: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("not a composer.json: {e}"))?;
    let path = p.path.clone();
    let mut skipped = 0usize;
    for section in ["require", "require-dev"] {
        let Some(map) = doc.get(section).and_then(serde_json::Value::as_object) else {
            continue;
        };
        for (name, req) in map {
            // `php`, `ext-*` and `lib-*` are platform requirements, not packages.
            if !name.contains('/') {
                continue;
            }
            let req = req.as_str().unwrap_or("");
            match requirement_floor(req) {
                Some(floor) => p.push(Dep::manifest(ECO, name, req, &floor, &path))?,
                None => {
                    p.push(Dep::manifest(ECO, name, "", "", &path))?;
                    skipped += 1;
                }
            }
        }
    }
    if skipped > 0 {
        p.note(format!(
            "{skipped} requirements are development branches or wildcards and are not checked"
        ));
    }
    Ok(())
}
