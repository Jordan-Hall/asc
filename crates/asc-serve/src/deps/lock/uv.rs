//! `uv.lock`.
//!
//! A registry package block holds the version, the source distribution and the wheels, each with its URL,
//! SHA-256, size and (in current files) upload time. Moving a pin writes the version and the files PyPI
//! lists for it, in uv's order (wheels by file name), and updates the `specifier` the project's own
//! `requires-dist` metadata records when the move changed the requirement.

use super::{LockIn, Requester, changed};
use crate::deps::facts::PyFile;
use crate::deps::model::Ecosystem;
use crate::deps::ranges::admits;
use crate::deps::rewrite::{Outcome, Pin};

fn canon(name: &str) -> String {
    Ecosystem::PyPi.canonical(name)
}

pub fn requesters(text: &str, pin: &Pin<'_>) -> Result<Vec<Requester>, String> {
    let doc: toml::Value =
        toml::from_str(text).map_err(|_| "uv.lock is not valid TOML".to_owned())?;
    let want = canon(pin.name);
    let mut out = Vec::new();
    for p in doc
        .get("package")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
    {
        // The workspace's own packages are the manifests' business.
        if p.get("source").and_then(|s| s.get("registry")).is_none() {
            continue;
        }
        let uses = p
            .get("dependencies")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .any(|d| {
                d.get("name")
                    .and_then(toml::Value::as_str)
                    .is_some_and(|n| canon(n) == want)
            });
        if uses {
            out.push(Requester {
                name: p
                    .get("name")
                    .and_then(toml::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                version: p
                    .get("version")
                    .and_then(toml::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                range: None,
            });
        }
    }
    Ok(out)
}

fn file_host(url: &str) -> &str {
    url.strip_prefix("https://")
        .and_then(|r| r.split('/').next())
        .unwrap_or("")
}

fn entry(f: &PyFile, with_time: bool, prefix: &str) -> Result<String, String> {
    let mut s = format!(
        "{prefix}{{ url = \"{}\", hash = \"sha256:{}\", size = {}",
        f.url, f.sha256, f.size
    );
    if with_time {
        if f.upload_time.is_empty() {
            return Err("PyPI gave no upload time for a file the lock records one for".into());
        }
        s.push_str(&format!(", upload-time = \"{}\"", f.upload_time));
    }
    s.push_str(" }");
    Ok(s)
}

pub fn rewrite(text: &str, c: &LockIn<'_>) -> Outcome {
    let pin = c.pin;
    let want = canon(pin.name);
    if text.contains('\r') {
        return Err("uv.lock has Windows line endings".into());
    }
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let heads: Vec<usize> = (0..lines.len())
        .filter(|&i| lines[i].trim_end() == "[[package]]")
        .collect();
    let mut out: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
    let mut found = false;
    let url_re =
        regex::Regex::new(r#"url = "([^"]+)", hash = "sha256:([0-9a-f]{64})""#).expect("valid");
    for &h in &heads {
        let end = (h + 1..lines.len())
            .find(|&i| lines[i].trim_end() == "[[package]]")
            .unwrap_or(lines.len());
        let top_end = (h + 1..end)
            .find(|&i| lines[i].starts_with('['))
            .unwrap_or(end);
        let get = |key: &str| {
            (h + 1..top_end).find_map(|i| {
                lines[i]
                    .strip_prefix(&format!("{key} = \""))
                    .and_then(|r| r.trim_end().strip_suffix('"'))
                    .map(|v| (i, v.to_owned()))
            })
        };
        let (Some((_, name)), Some((vi, version))) = (get("name"), get("version")) else {
            continue;
        };
        if canon(&name) != want || version != pin.from {
            continue;
        }
        let registry = (h + 1..top_end).any(|i| lines[i].starts_with("source = { registry = "));
        if !registry {
            return Err(format!(
                "{} is locked from a source other than a registry",
                pin.name
            ));
        }
        found = true;
        out[vi] = format!("version = \"{}\"\n", pin.to);
        let sdist = (h + 1..top_end).find(|&i| lines[i].starts_with("sdist = "));
        let wheels_start = (h + 1..top_end).find(|&i| lines[i].trim_end() == "wheels = [");
        let wheels_end =
            wheels_start.and_then(|s| (s + 1..top_end).find(|&i| lines[i].trim_end() == "]"));
        // What the lock records now, to be checked against the registry and to set the style.
        let mut old_lines: Vec<&str> = Vec::new();
        if let Some(s) = sdist {
            old_lines.push(lines[s]);
        }
        if let (Some(s), Some(e)) = (wheels_start, wheels_end) {
            old_lines.extend(&lines[s + 1..e]);
        }
        let with_time = old_lines.iter().any(|l| l.contains("upload-time"));
        for l in &old_lines {
            if let Some(m) = url_re.captures(l) {
                let fname = m[1].rsplit('/').next().unwrap_or("");
                if let Some(f) = c.from.files.iter().find(|f| f.filename == fname)
                    && (f.sha256 != m[2] || f.url != m[1])
                {
                    return Err(format!(
                        "the locked hash or URL of {fname} is not the one PyPI publishes for it"
                    ));
                }
                if let Some(nf) = c.to.files.first()
                    && file_host(&m[1]) != file_host(&nf.url)
                {
                    return Err(format!(
                        "{} is locked from a host other than the one PyPI serves files from",
                        pin.name
                    ));
                }
            }
        }
        let new_sdist = c.to.files.iter().find(|f| f.sdist);
        let mut wheels: Vec<&PyFile> = c.to.files.iter().filter(|f| !f.sdist).collect();
        wheels.sort_by(|a, b| a.filename.cmp(&b.filename));
        // Rewrite the two blocks in place; a new sdist goes where the wheels were.
        let sdist_text = match new_sdist {
            Some(f) => Some(format!(
                "sdist = {}\n",
                entry(f, with_time, "")?.trim_start()
            )),
            None => None,
        };
        let mut wheels_text = String::new();
        if !wheels.is_empty() {
            wheels_text.push_str("wheels = [\n");
            for w in wheels {
                wheels_text.push_str(&format!("{},\n", entry(w, with_time, "    ")?));
            }
            wheels_text.push_str("]\n");
        }
        match (sdist, wheels_start, wheels_end) {
            (Some(s), Some(ws), Some(we)) => {
                out[s] = sdist_text.unwrap_or_default();
                for o in out.iter_mut().take(we + 1).skip(ws) {
                    o.clear();
                }
                out[ws] = wheels_text;
            }
            (Some(s), None, _) => {
                out[s] = format!("{}{wheels_text}", sdist_text.unwrap_or_default());
            }
            (None, Some(ws), Some(we)) => {
                for o in out.iter_mut().take(we + 1).skip(ws) {
                    o.clear();
                }
                out[ws] = format!("{}{wheels_text}", sdist_text.unwrap_or_default());
            }
            _ => {
                // Neither recorded: the files go after the last top-level line of the block.
                let at = (h + 1..top_end)
                    .rev()
                    .find(|&i| !lines[i].trim().is_empty())
                    .unwrap_or(h)
                    + 1;
                let add = format!("{}{wheels_text}", sdist_text.unwrap_or_default());
                out[at - 1].push_str(&add);
            }
        }
    }
    if !found {
        return Ok(None);
    }
    // The project's own `requires-dist` and `requires-dev` carry the specifiers of its manifest.
    let spec_re =
        regex::Regex::new(r#"(\{[^{}]*\bname = "([^"]+)"[^{}]*\bspecifier = ")([^"]*)(")"#)
            .map_err(|e| e.to_string())?;
    let want_name = canon(pin.name);
    for l in out.iter_mut() {
        if !l.contains("specifier = ") {
            continue;
        }
        let Some(cap) = spec_re.captures(l) else {
            continue;
        };
        if canon(&cap[2]) != want_name {
            continue;
        }
        let spec = cap[3].to_owned();
        match (
            admits(Ecosystem::PyPi, &spec, pin.from),
            admits(Ecosystem::PyPi, &spec, pin.to),
        ) {
            (_, Some(true)) => {}
            (Some(true), Some(false)) => {
                let req = c.requirement.ok_or_else(|| {
                    format!(
                        "the specifier {spec} of {} does not admit {}, and the manifest did not change",
                        pin.name, pin.to
                    )
                })?;
                let req: String = req.chars().filter(|ch| !ch.is_whitespace()).collect();
                let m = cap.get(3).ok_or("no specifier")?;
                l.replace_range(m.range(), &req);
            }
            _ => {
                return Err(format!(
                    "the specifier {spec} of {} is not one this code can move to {}",
                    pin.name, pin.to
                ));
            }
        }
    }
    changed(text, out.concat())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::facts::FactStore;
    use crate::deps::facts::Facts;

    fn f(name: &str, sha: char, sdist: bool) -> PyFile {
        PyFile {
            filename: name.into(),
            url: format!("https://files.pythonhosted.org/packages/aa/{name}"),
            sha256: sha.to_string().repeat(64),
            size: 10,
            upload_time: "2024-05-20T21:33:25.928Z".into(),
            sdist,
        }
    }

    fn facts(v: &str, sha: char) -> Facts {
        Facts {
            files: vec![
                f(&format!("click-{v}-py3-none-any.whl"), sha, false),
                f(&format!("click-{v}.tar.gz"), sha, true),
            ],
            ..Facts::default()
        }
    }

    fn lock(v: &str, sha: char, spec: &str) -> String {
        let s = sha.to_string().repeat(64);
        format!(
            "version = 1\nrequires-python = \">=3.9\"\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\nsource = {{ editable = \".\" }}\ndependencies = [\n    {{ name = \"click\" }},\n]\n\n[package.metadata]\nrequires-dist = [{{ name = \"click\", specifier = \"{spec}\" }}]\n\n[[package]]\nname = \"click\"\nversion = \"{v}\"\nsource = {{ registry = \"https://pypi.org/simple\" }}\nsdist = {{ url = \"https://files.pythonhosted.org/packages/aa/click-{v}.tar.gz\", hash = \"sha256:{s}\", size = 10, upload-time = \"2024-05-20T21:33:25.928Z\" }}\nwheels = [\n    {{ url = \"https://files.pythonhosted.org/packages/aa/click-{v}-py3-none-any.whl\", hash = \"sha256:{s}\", size = 10, upload-time = \"2024-05-20T21:33:25.928Z\" }},\n]\n\n[[package]]\nname = \"zzz\"\nversion = \"1.0\"\nsource = {{ registry = \"https://pypi.org/simple\" }}\ndependencies = [\n    {{ name = \"click\" }},\n]\n"
        )
    }

    fn pin() -> Pin<'static> {
        Pin {
            eco: Ecosystem::PyPi,
            name: "click",
            from: "8.1.0",
            to: "8.1.7",
        }
    }

    #[test]
    fn files_and_version_are_replaced_and_the_manifest_specifier_follows_a_new_requirement() {
        let (from, to) = (facts("8.1.0", 'a'), facts("8.1.7", 'b'));
        let store = FactStore::new();
        let p = pin();
        let c = LockIn {
            pin: &p,
            from: &from,
            to: &to,
            requirement: None,
            manifest: None,
            store: &store,
        };
        let got = rewrite(&lock("8.1.0", 'a', ">=8"), &c)
            .unwrap()
            .unwrap()
            .text;
        assert_eq!(got, lock("8.1.7", 'b', ">=8"));
        // A requirement that no longer admits the version is replaced by the manifest's new one.
        let p2 = Pin {
            to: "9.0.0",
            ..pin()
        };
        let to2 = facts("9.0.0", 'c');
        let c2 = LockIn {
            pin: &p2,
            to: &to2,
            requirement: Some("== 9.0.0"),
            ..c
        };
        let got = rewrite(&lock("8.1.0", 'a', "==8.1.0"), &c2)
            .unwrap()
            .unwrap()
            .text;
        assert_eq!(got, lock("9.0.0", 'c', "==9.0.0"));
        let c3 = LockIn {
            requirement: None,
            ..c2
        };
        assert!(
            rewrite(&lock("8.1.0", 'a', "==8.1.0"), &c3)
                .unwrap_err()
                .contains("did not change")
        );
    }

    #[test]
    fn a_wrong_digest_a_foreign_host_or_a_git_source_stops_the_move() {
        let (from, to) = (facts("8.1.0", 'z'), facts("8.1.7", 'b'));
        let store = FactStore::new();
        let p = pin();
        let c = LockIn {
            pin: &p,
            from: &from,
            to: &to,
            requirement: None,
            manifest: None,
            store: &store,
        };
        assert!(
            rewrite(&lock("8.1.0", 'a', ">=8"), &c)
                .unwrap_err()
                .contains("PyPI publishes")
        );
        let from = facts("8.1.0", 'a');
        let c = LockIn { from: &from, ..c };
        let foreign = lock("8.1.0", 'a', ">=8").replace("files.pythonhosted.org", "mirror.example");
        assert!(
            rewrite(&foreign, &c)
                .unwrap_err()
                .contains("PyPI publishes")
        );
        let git = lock("8.1.0", 'a', ">=8").replace(
            "source = { registry = \"https://pypi.org/simple\" }\nsdist",
            "source = { git = \"https://x/y\" }\nsdist",
        );
        assert!(
            rewrite(&git, &c)
                .unwrap_err()
                .contains("other than a registry")
        );
    }

    #[test]
    fn registry_dependents_are_requesters() {
        let r = requesters(&lock("8.1.0", 'a', ">=8"), &pin()).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "zzz");
    }
}
