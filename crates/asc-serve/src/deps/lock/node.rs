//! `package-lock.json`: who requires a package.

use super::Requester;
use crate::deps::rewrite::{Pin, find, members, root_members};

const SECTIONS: [&str; 3] = ["dependencies", "optionalDependencies", "peerDependencies"];

fn string_of(text: &str, ms: &[crate::deps::rewrite::Member], key: &str) -> String {
    find(ms, key)
        .filter(|m| m.is_str())
        .map(|m| m.string(text).to_owned())
        .unwrap_or_default()
}

fn legacy(
    text: &str,
    open: usize,
    pin: &Pin<'_>,
    out: &mut Vec<Requester>,
    depth: usize,
) -> Result<(), String> {
    if depth > 64 {
        return Err("the dependency tree is nested too deeply".into());
    }
    let Some(ms) = members(text, open) else {
        return Ok(());
    };
    for m in ms.iter().filter(|m| m.is_obj()) {
        let Some(entry) = members(text, m.val_start) else {
            continue;
        };
        if let Some(req) = find(&entry, "requires").filter(|r| r.is_obj())
            && let Some(rs) = members(text, req.val_start)
            && let Some(r) = find(&rs, pin.name).filter(|r| r.is_str())
        {
            out.push(Requester {
                name: m.key.clone(),
                version: string_of(text, &entry, "version"),
                range: Some(r.string(text).to_owned()),
            });
        }
        if let Some(deps) = find(&entry, "dependencies").filter(|d| d.is_obj()) {
            legacy(text, deps.val_start, pin, out, depth + 1)?;
        }
    }
    Ok(())
}

/// The installed packages of the lock whose dependency maps name the pin, with the range they record.
/// The root and the workspace members are the manifests' business, not the lock's.
pub fn package_lock_requesters(text: &str, pin: &Pin<'_>) -> Result<Vec<Requester>, String> {
    let top = root_members(text)?;
    let mut out = Vec::new();
    if let Some(packages) = find(&top, "packages").filter(|m| m.is_obj()) {
        for m in members(text, packages.val_start)
            .unwrap_or_default()
            .iter()
            .filter(|m| m.is_obj() && m.key.contains("node_modules/"))
        {
            let Some(entry) = members(text, m.val_start) else {
                continue;
            };
            let name = m
                .key
                .rsplit("node_modules/")
                .next()
                .unwrap_or(&m.key)
                .to_owned();
            for section in SECTIONS {
                let Some(deps) = find(&entry, section).filter(|d| d.is_obj()) else {
                    continue;
                };
                let deps = members(text, deps.val_start).unwrap_or_default();
                if let Some(r) = find(&deps, pin.name).filter(|r| r.is_str()) {
                    out.push(Requester {
                        name: name.clone(),
                        version: string_of(text, &entry, "version"),
                        range: Some(r.string(text).to_owned()),
                    });
                }
            }
        }
    }
    if let Some(tree) = find(&top, "dependencies").filter(|m| m.is_obj()) {
        legacy(text, tree.val_start, pin, &mut out, 0)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::model::Ecosystem;

    #[test]
    fn dependents_are_read_from_the_installed_packages_with_their_ranges() {
        let lock = r#"{"lockfileVersion":3,"packages":{
            "":{"dependencies":{"left-pad":"^1.0.0"}},
            "node_modules/a":{"version":"1.0.0","dependencies":{"left-pad":"~1.0.0"}},
            "node_modules/b/node_modules/c":{"version":"2.0.0","peerDependencies":{"left-pad":">=1"}},
            "node_modules/left-pad":{"version":"1.0.0"}}}"#;
        let pin = Pin {
            eco: Ecosystem::Npm,
            name: "left-pad",
            from: "1.0.0",
            to: "1.3.0",
        };
        let r = package_lock_requesters(lock, &pin).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(
            r[0],
            Requester {
                name: "a".into(),
                version: "1.0.0".into(),
                range: Some("~1.0.0".into())
            }
        );
        assert_eq!(r[1].name, "c");
        assert_eq!(r[1].range.as_deref(), Some(">=1"));
        let v1 = r#"{"lockfileVersion":1,"dependencies":{"a":{"version":"1.0.0","requires":{"left-pad":"^1.0.0"},
            "dependencies":{"b":{"version":"2.0.0","requires":{"left-pad":"1.x"}}}}}}"#;
        let r = package_lock_requesters(v1, &pin).unwrap();
        assert_eq!(
            r.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
    }
}
