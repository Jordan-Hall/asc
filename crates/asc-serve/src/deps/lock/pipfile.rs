//! `Pipfile.lock`.
//!
//! Each package is a `"version": "==x"` and the `"hashes"` of every file of that release. Moving a pin
//! writes both, the hashes sorted as pipenv writes them. The lock's own `_meta.hash` is of the `Pipfile`,
//! which a move does not edit, so the `Pipfile` has to admit the new version already: if it does not,
//! the move is for a person (or `pipenv`) to make.

use super::{LockIn, Requester, changed};
use crate::deps::model::Ecosystem;
use crate::deps::parse::parse_file;
use crate::deps::ranges::admits;
use crate::deps::rewrite::{Edits, Member, Outcome, Pin, find, members, root_members};

const SECTIONS: [&str; 2] = ["default", "develop"];

fn canon(name: &str) -> String {
    Ecosystem::PyPi.canonical(name)
}

/// Every other pinned package: the lock records no edges, so any of them may be the one that needs it.
pub fn requesters(text: &str, pin: &Pin<'_>) -> Result<Vec<Requester>, String> {
    let top = root_members(text)?;
    let want = canon(pin.name);
    let mut out = Vec::new();
    for s in SECTIONS {
        let Some(sec) = find(&top, s).filter(|m| m.is_obj()) else {
            continue;
        };
        for m in members(text, sec.val_start).unwrap_or_default() {
            if canon(&m.key) == want || !m.is_obj() {
                continue;
            }
            let entry = members(text, m.val_start).unwrap_or_default();
            if let Some(v) = find(&entry, "version").filter(|v| v.is_str())
                && let Some(v) = v.string(text).strip_prefix("==")
            {
                out.push(Requester {
                    name: m.key.clone(),
                    version: v.to_owned(),
                    range: None,
                });
            }
        }
    }
    Ok(out)
}

fn member_str(text: &str, ms: &[Member], key: &str) -> String {
    find(ms, key)
        .filter(|m| m.is_str())
        .map(|m| m.string(text).to_owned())
        .unwrap_or_default()
}

/// The indent of the line holding `at`.
fn indent_at(text: &str, at: usize) -> usize {
    let start = text[..at].rfind('\n').map_or(0, |p| p + 1);
    text[start..].len() - text[start..].trim_start().len()
}

pub fn rewrite(text: &str, c: &LockIn<'_>) -> Outcome {
    let pin = c.pin;
    let (pipfile, _) = c
        .manifest
        .ok_or("there is no Pipfile beside Pipfile.lock to hold the requirement")?;
    // The Pipfile has to admit the new version, since it is not edited.
    let parsed = parse_file("Pipfile", pipfile.as_bytes())
        .ok_or("the Pipfile is not readable")?
        .map_err(|e| format!("the Pipfile is not readable: {e}"))?;
    let want = canon(pin.name);
    for d in parsed.deps.iter().filter(|d| canon(&d.name) == want) {
        let req = d.requirement.trim();
        if !(req.is_empty() || req == "*") && admits(Ecosystem::PyPi, req, pin.to) != Some(true) {
            return Err(format!(
                "the Pipfile requires {} {req}, which {} does not satisfy, and the Pipfile is not edited",
                pin.name, pin.to
            ));
        }
    }
    let top = root_members(text)?;
    let mut edits = Edits::new();
    let mut found = false;
    for s in SECTIONS {
        let Some(sec) = find(&top, s).filter(|m| m.is_obj()) else {
            continue;
        };
        for m in members(text, sec.val_start)
            .unwrap_or_default()
            .iter()
            .filter(|m| m.is_obj() && canon(&m.key) == want)
        {
            let entry = members(text, m.val_start).unwrap_or_default();
            let Some(v) = find(&entry, "version").filter(|v| v.is_str()) else {
                continue;
            };
            if v.string(text) != format!("=={}", pin.from) {
                continue;
            }
            found = true;
            v.set(&mut edits, &format!("=={}", pin.to));
            if let Some(h) = find(&entry, "hashes").filter(|h| h.is_arr()) {
                // Hashes the lock holds for files the registry also lists must be the registry's.
                let old = &text[h.val_start..h.val_end];
                for line in old.split('"').filter(|s| s.starts_with("sha256:")) {
                    let sha = line.trim_start_matches("sha256:");
                    if let Some(f) = c.from.files.iter().find(|f| f.sha256 == sha) {
                        let _ = f;
                    } else if !c.from.files.is_empty() {
                        return Err(format!(
                            "the locked hash {sha} of {} {} is not one of the files PyPI lists for it",
                            pin.name, pin.from
                        ));
                    }
                }
                let ind = indent_at(text, h.val_start);
                // The array's own line indent is that of its key.
                let key_indent = ind;
                let mut hashes: Vec<String> =
                    c.to.files
                        .iter()
                        .map(|f| format!("sha256:{}", f.sha256))
                        .collect();
                hashes.sort();
                hashes.dedup();
                let items: Vec<String> = hashes
                    .iter()
                    .map(|x| format!("{}\"{x}\"", " ".repeat(key_indent + 4)))
                    .collect();
                edits.push(
                    h.val_start,
                    h.val_end,
                    format!("[\n{}\n{}]", items.join(",\n"), " ".repeat(key_indent)),
                );
            }
            let _ = member_str(text, &entry, "index");
        }
    }
    if !found {
        return Ok(None);
    }
    changed(text, edits.apply(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::facts::FactStore;
    use crate::deps::facts::{Facts, PyFile};

    fn facts(sha: &[char]) -> Facts {
        Facts {
            files: sha
                .iter()
                .enumerate()
                .map(|(i, s)| PyFile {
                    filename: format!("f{i}"),
                    url: "https://x/f".into(),
                    sha256: s.to_string().repeat(64),
                    size: 1,
                    upload_time: String::new(),
                    sdist: false,
                })
                .collect(),
            ..Facts::default()
        }
    }

    fn lock(version: &str, hashes: &[char]) -> String {
        let items: Vec<String> = hashes
            .iter()
            .map(|h| format!("                \"sha256:{}\"", h.to_string().repeat(64)))
            .collect();
        format!(
            "{{\n    \"_meta\": {{\n        \"hash\": {{\n            \"sha256\": \"abc\"\n        }}\n    }},\n    \"default\": {{\n        \"requests\": {{\n            \"hashes\": [\n{}\n            ],\n            \"index\": \"pypi\",\n            \"version\": \"=={version}\"\n        }},\n        \"idna\": {{\n            \"version\": \"==3.4\"\n        }}\n    }},\n    \"develop\": {{}}\n}}\n",
            items.join(",\n")
        )
    }

    fn pin() -> Pin<'static> {
        Pin {
            eco: Ecosystem::PyPi,
            name: "requests",
            from: "2.28.1",
            to: "2.31.0",
        }
    }

    #[test]
    fn version_and_sorted_hashes_are_replaced() {
        let (from, to) = (facts(&['a', 'b']), facts(&['d', 'c']));
        let store = FactStore::new();
        let p = pin();
        let pf = "[packages]\nrequests = \"*\"\n";
        let c = LockIn {
            pin: &p,
            from: &from,
            to: &to,
            requirement: None,
            manifest: Some((pf, pf)),
            store: &store,
        };
        let got = rewrite(&lock("2.28.1", &['a', 'b']), &c)
            .unwrap()
            .unwrap()
            .text;
        assert_eq!(got, lock("2.31.0", &['c', 'd']));
    }

    #[test]
    fn a_pipfile_that_does_not_admit_the_version_or_a_foreign_hash_stops_the_move() {
        let (from, to) = (facts(&['a', 'b']), facts(&['c']));
        let store = FactStore::new();
        let p = pin();
        let pf = "[packages]\nrequests = \"==2.28.1\"\n";
        let c = LockIn {
            pin: &p,
            from: &from,
            to: &to,
            requirement: None,
            manifest: Some((pf, pf)),
            store: &store,
        };
        assert!(
            rewrite(&lock("2.28.1", &['a', 'b']), &c)
                .unwrap_err()
                .contains("not edited")
        );
        let pf2 = "[packages]\nrequests = \">=2\"\n";
        let c = LockIn {
            manifest: Some((pf2, pf2)),
            ..c
        };
        assert!(
            rewrite(&lock("2.28.1", &['a', 'z']), &c)
                .unwrap_err()
                .contains("not one of the files")
        );
        assert!(
            rewrite(
                &lock("2.28.1", &['a', 'b']),
                &LockIn {
                    manifest: None,
                    ..c
                }
            )
            .unwrap_err()
            .contains("no Pipfile")
        );
    }

    #[test]
    fn every_other_pinned_package_is_a_requester() {
        let r = requesters(&lock("2.28.1", &['a']), &pin()).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].name.as_str(), r[0].version.as_str()), ("idna", "3.4"));
    }
}
