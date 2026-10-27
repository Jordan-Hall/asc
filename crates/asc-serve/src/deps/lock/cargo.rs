//! `Cargo.lock`: who depends on a crate.

use super::Requester;
use crate::deps::rewrite::Pin;

fn same_crate(a: &str, b: &str) -> bool {
    a.replace('_', "-")
        .eq_ignore_ascii_case(&b.replace('_', "-"))
}

/// The registry packages of the lock whose `dependencies` name the pin at its old version. Packages
/// without a source are the workspace's own and are the manifests' business; a git source cannot be
/// asked what it requires, so it stops the move.
pub fn requesters(text: &str, pin: &Pin<'_>) -> Result<Vec<Requester>, String> {
    let doc: toml::Value =
        toml::from_str(text).map_err(|_| "Cargo.lock is not valid TOML".to_owned())?;
    let mut out = Vec::new();
    for p in doc
        .get("package")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
    {
        let (Some(name), Some(version)) = (
            p.get("name").and_then(toml::Value::as_str),
            p.get("version").and_then(toml::Value::as_str),
        ) else {
            continue;
        };
        let depends = p
            .get("dependencies")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_str)
            .any(|d| {
                let mut it = d.split(' ');
                let dep = it.next().unwrap_or("");
                same_crate(dep, pin.name) && it.next().is_none_or(|v| v == pin.from)
            });
        if !depends {
            continue;
        }
        match p.get("source").and_then(toml::Value::as_str) {
            None => {}
            Some(s) if s.starts_with("git+") => {
                return Err(format!(
                    "{name} {version} is a git dependency that depends on {}; what it requires cannot be read here",
                    pin.name
                ));
            }
            Some(_) => out.push(Requester {
                name: name.to_owned(),
                version: version.to_owned(),
                range: None,
            }),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::model::Ecosystem;

    const LOCK: &str = "version = 3\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"serde 1.0.152\",\n \"other\",\n]\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.152\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"aa\"\n\n[[package]]\nname = \"serde_json\"\nversion = \"1.0.90\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"bb\"\ndependencies = [\n \"serde 1.0.152\",\n]\n\n[[package]]\nname = \"old\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\ndependencies = [\n \"serde 0.9.0\",\n]\n";

    fn pin() -> Pin<'static> {
        Pin {
            eco: Ecosystem::CratesIo,
            name: "serde",
            from: "1.0.152",
            to: "1.0.200",
        }
    }

    #[test]
    fn registry_dependents_of_that_version_are_requesters_and_workspace_members_are_not() {
        let r = requesters(LOCK, &pin()).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(
            (r[0].name.as_str(), r[0].version.as_str()),
            ("serde_json", "1.0.90")
        );
        assert!(r[0].range.is_none());
    }

    #[test]
    fn a_git_dependent_stops_the_move() {
        let lock = LOCK.replace(
            "name = \"old\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\ndependencies = [\n \"serde 0.9.0\"",
            "name = \"old\"\nversion = \"0.1.0\"\nsource = \"git+https://example.com/old\"\ndependencies = [\n \"serde\"",
        );
        assert!(
            requesters(&lock, &pin())
                .unwrap_err()
                .contains("git dependency")
        );
    }
}
