//! `poetry.lock`.
//!
//! A package block holds the version and the list of its release files with their SHA-256 (in `files`
//! since Poetry 1.5, in the `[metadata.files]` table before). Moving a pin writes the new version and the
//! files PyPI lists for it, and, when the move also edited `pyproject.toml`, the new `content-hash`.
//! The dependency tables of the block stay as they are: the registry has shown the new version declares
//! the same ones.

use super::{LockIn, Requester, changed, hashes, toml_str};
use crate::deps::facts::PyFile;
use crate::deps::model::Ecosystem;
use crate::deps::rewrite::{Outcome, Pin};

fn canon(name: &str) -> String {
    Ecosystem::PyPi.canonical(name)
}

/// A Poetry constraint (`^1.2`, `~1.2.3`, `>=1,<2`, `*`, `1.2.*`, `1.2.3`) as a PEP 440 specifier set.
/// `None` for what has no such equivalent (`||`).
pub fn to_pep440(c: &str) -> Option<String> {
    let c = c.trim();
    if c.contains("||") || c.contains(" or ") {
        return None;
    }
    let mut out = Vec::new();
    for part in c.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let one = |op: &str, rest: &str| -> Option<String> {
            let nums: Vec<u64> = rest
                .trim()
                .split('.')
                .map(|n| n.parse().ok())
                .collect::<Option<Vec<_>>>()?;
            if nums.is_empty() {
                return None;
            }
            let text = |v: &[u64]| v.iter().map(u64::to_string).collect::<Vec<_>>().join(".");
            let upper = |at: usize| {
                let mut v: Vec<u64> = nums
                    .iter()
                    .copied()
                    .chain(std::iter::repeat(0))
                    .take(at + 1)
                    .collect();
                v[at] += 1;
                text(&v)
            };
            Some(match op {
                "^" => {
                    let at = nums.iter().position(|n| *n != 0).unwrap_or(nums.len() - 1);
                    format!(">={},<{}", text(&nums), upper(at))
                }
                "~" => format!(">={},<{}", text(&nums), upper(usize::from(nums.len() >= 2))),
                _ => return None,
            })
        };
        if part == "*" {
            continue;
        }
        if let Some(r) = part.strip_prefix('^') {
            out.push(one("^", r)?);
        } else if let Some(r) = part.strip_prefix('~').filter(|r| !r.starts_with('=')) {
            out.push(one("~", r)?);
        } else if part.starts_with(|c: char| c.is_ascii_digit()) {
            out.push(format!("=={part}"));
        } else {
            out.push(part.to_owned());
        }
    }
    Some(out.join(","))
}

pub fn requesters(text: &str, pin: &Pin<'_>) -> Result<Vec<Requester>, String> {
    let doc: toml::Value =
        toml::from_str(text).map_err(|_| "poetry.lock is not valid TOML".to_owned())?;
    let want = canon(pin.name);
    let mut out = Vec::new();
    for p in doc
        .get("package")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = p.get("name").and_then(toml::Value::as_str).unwrap_or("");
        let version = p.get("version").and_then(toml::Value::as_str).unwrap_or("");
        let Some(deps) = p.get("dependencies").and_then(toml::Value::as_table) else {
            continue;
        };
        for (dep, spec) in deps {
            if canon(dep) != want {
                continue;
            }
            let specs: Vec<&toml::Value> = match spec {
                toml::Value::Array(a) => a.iter().collect(),
                one => vec![one],
            };
            for s in specs {
                let raw = match s {
                    toml::Value::String(v) => v.clone(),
                    toml::Value::Table(t) => t
                        .get("version")
                        .and_then(toml::Value::as_str)
                        .unwrap_or("*")
                        .to_owned(),
                    _ => {
                        return Err(format!(
                            "{name} requires {dep} in a way this code cannot read"
                        ));
                    }
                };
                let range = to_pep440(&raw).ok_or_else(|| {
                    format!("{name} requires {dep} {raw:?}, which is not a constraint this code can evaluate")
                })?;
                out.push(Requester {
                    name: name.to_owned(),
                    version: version.to_owned(),
                    range: Some(range),
                });
            }
        }
    }
    Ok(out)
}

fn file_line(f: &PyFile, spaced: bool) -> String {
    if spaced {
        format!(
            "    {{ file = \"{}\", hash = \"sha256:{}\" }},\n",
            toml_str(&f.filename),
            toml_str(&f.sha256)
        )
    } else {
        format!(
            "    {{file = \"{}\", hash = \"sha256:{}\"}},\n",
            toml_str(&f.filename),
            toml_str(&f.sha256)
        )
    }
}

/// `(filename, sha256)` of the file lines in `old`.
fn old_files(old: &[&str]) -> Vec<(String, String)> {
    let r = regex::Regex::new(r#"file\s*=\s*"([^"]+)",\s*hash\s*=\s*"sha256:([0-9a-f]{64})""#)
        .expect("a valid pattern");
    old.iter()
        .filter_map(|l| r.captures(l).map(|c| (c[1].to_owned(), c[2].to_owned())))
        .collect()
}

fn check_old(old: &[(String, String)], c: &LockIn<'_>) -> Result<(), String> {
    for (name, sha) in old {
        if let Some(f) = c.from.files.iter().find(|f| f.filename == *name)
            && f.sha256 != *sha
        {
            return Err(format!(
                "the locked hash of {name} is not the one PyPI publishes for it"
            ));
        }
    }
    Ok(())
}

pub fn rewrite(text: &str, c: &LockIn<'_>) -> Outcome {
    let pin = c.pin;
    let want = canon(pin.name);
    if text.contains('\r') {
        return Err("poetry.lock has Windows line endings".into());
    }
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let is_head = |l: &str| l.starts_with("[[package]]") || l.starts_with("[metadata");
    let heads: Vec<usize> = (0..lines.len())
        .filter(|&i| lines[i].trim_end() == "[[package]]")
        .collect();
    let mut out: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
    let mut found = false;
    let mut legacy_name: Option<String> = None;
    for &h in &heads {
        let end = (h + 1..lines.len())
            .find(|&i| is_head(lines[i]))
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
        if lines[top_end..end]
            .iter()
            .any(|l| l.starts_with("[package.source]"))
        {
            return Err(format!(
                "{} is locked from a source other than PyPI",
                pin.name
            ));
        }
        found = true;
        out[vi] = format!("version = \"{}\"\n", pin.to);
        // Poetry 1.5 and later: `files = [` ... `]` in the block.
        if let Some(fs) = (h + 1..top_end).find(|&i| lines[i].trim_end() == "files = [") {
            let fe = (fs + 1..top_end)
                .find(|&i| lines[i].trim_end() == "]")
                .ok_or("a files list of poetry.lock is not closed")?;
            let old = old_files(&lines[fs + 1..fe]);
            check_old(&old, c)?;
            let spaced = lines[fs + 1..fe].iter().any(|l| l.contains("{ file"));
            let mut new = String::from("files = [\n");
            for f in &c.to.files {
                new.push_str(&file_line(f, spaced));
            }
            new.push_str("]\n");
            for o in out.iter_mut().take(fe + 1).skip(fs) {
                o.clear();
            }
            out[fs] = new;
        } else {
            legacy_name = Some(name);
        }
    }
    if !found {
        return Ok(None);
    }
    // Before Poetry 1.5: `[metadata.files]` with a list per package.
    if let Some(name) = legacy_name {
        let table = lines
            .iter()
            .position(|l| l.trim_end() == "[metadata.files]")
            .ok_or_else(|| format!("poetry.lock lists no files for {}", pin.name))?;
        let key = format!("{name} = [");
        let ks = (table + 1..lines.len())
            .find(|&i| {
                lines[i].trim_end() == key || lines[i].trim_end() == format!("\"{name}\" = [")
            })
            .ok_or_else(|| format!("[metadata.files] has no list for {}", pin.name))?;
        let ke = (ks + 1..lines.len())
            .find(|&i| lines[i].trim_end() == "]")
            .ok_or("a files list of poetry.lock is not closed")?;
        let old = old_files(&lines[ks + 1..ke]);
        check_old(&old, c)?;
        let spaced = lines[ks + 1..ke].iter().any(|l| l.contains("{ file"));
        let mut new = format!("{}\n", lines[ks].trim_end());
        for f in &c.to.files {
            new.push_str(&file_line(f, spaced));
        }
        new.push_str("]\n");
        for o in out.iter_mut().take(ke + 1).skip(ks) {
            o.clear();
        }
        out[ks] = new;
    }
    let mut new_text: String = out.concat();
    if let Some((old, new)) = c.manifest
        && old != new
    {
        new_text = hashes::set_poetry_hash(&new_text, old, new)?;
    }
    changed(text, new_text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::facts::FactStore;
    use crate::deps::facts::Facts;

    fn file(name: &str, sha: char) -> PyFile {
        PyFile {
            filename: name.into(),
            url: format!("https://files.pythonhosted.org/{name}"),
            sha256: sha.to_string().repeat(64),
            size: 1,
            upload_time: String::new(),
            sdist: name.ends_with(".tar.gz"),
        }
    }

    fn facts(v: &str, sha: char) -> Facts {
        Facts {
            files: vec![
                file(&format!("six-{v}-py2.py3-none-any.whl"), sha),
                file(&format!("six-{v}.tar.gz"), sha),
            ],
            ..Facts::default()
        }
    }

    fn pin() -> Pin<'static> {
        Pin {
            eco: Ecosystem::PyPi,
            name: "Six",
            from: "1.15.0",
            to: "1.16.0",
        }
    }

    fn lock(spaced: bool, v: &str, sha: char) -> String {
        let (open, close) = if spaced { ("{ ", " }") } else { ("{", "}") };
        let f = |n: &str| {
            format!(
                "    {open}file = \"{n}\", hash = \"sha256:{}\"{close},\n",
                sha.to_string().repeat(64)
            )
        };
        format!(
            "# This file is automatically @generated by Poetry 1.8.2 and should not be changed by hand.\n\n[[package]]\nname = \"six\"\nversion = \"{v}\"\ndescription = \"Python 2 and 3 compatibility utilities\"\noptional = false\npython-versions = \">=2.7, !=3.0.*\"\nfiles = [\n{}{}]\n\n[[package]]\nname = \"other\"\nversion = \"1.0\"\ndescription = \"\"\noptional = false\npython-versions = \"*\"\nfiles = []\n\n[package.dependencies]\nsix = \">=1.5\"\n\n[metadata]\nlock-version = \"2.0\"\npython-versions = \">=3.8\"\ncontent-hash = \"abc\"\n",
            f(&format!("six-{v}-py2.py3-none-any.whl")),
            f(&format!("six-{v}.tar.gz")),
        )
    }

    #[test]
    fn the_version_and_the_files_are_replaced_in_the_style_of_the_file() {
        let (from, to) = (facts("1.15.0", 'a'), facts("1.16.0", 'b'));
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
        for spaced in [false, true] {
            let got = rewrite(&lock(spaced, "1.15.0", 'a'), &c)
                .unwrap()
                .unwrap()
                .text;
            assert_eq!(got, lock(spaced, "1.16.0", 'b'), "spaced={spaced}");
        }
    }

    #[test]
    fn a_hash_other_than_pypis_or_another_source_stops_the_move() {
        let (from, to) = (facts("1.15.0", 'c'), facts("1.16.0", 'b'));
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
            rewrite(&lock(false, "1.15.0", 'a'), &c)
                .unwrap_err()
                .contains("not the one PyPI")
        );
        let from = facts("1.15.0", 'a');
        let c = LockIn { from: &from, ..c };
        let sourced = lock(false, "1.15.0", 'a').replace(
            "[metadata]",
            "[package.source]\ntype = \"legacy\"\n\n[metadata]",
        );
        // The source table belongs to the last block here, so put it on six's.
        let sourced2 = lock(false, "1.15.0", 'a').replacen(
            "]\n\n[[package]]\nname = \"other\"",
            "]\n\n[package.source]\ntype = \"legacy\"\n\n[[package]]\nname = \"other\"",
            1,
        );
        let _ = sourced;
        assert!(
            rewrite(&sourced2, &c)
                .unwrap_err()
                .contains("other than PyPI")
        );
    }

    #[test]
    fn the_legacy_metadata_files_table_is_rewritten_too() {
        let old = "[[package]]\nname = \"six\"\nversion = \"1.15.0\"\ndescription = \"x\"\ncategory = \"main\"\noptional = false\npython-versions = \"*\"\n\n[metadata]\nlock-version = \"1.1\"\npython-versions = \"*\"\ncontent-hash = \"abc\"\n\n[metadata.files]\nsix = [\n    {file = \"six-1.15.0-py2.py3-none-any.whl\", hash = \"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"},\n    {file = \"six-1.15.0.tar.gz\", hash = \"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"},\n]\n";
        let (from, to) = (facts("1.15.0", 'a'), facts("1.16.0", 'b'));
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
        let got = rewrite(old, &c).unwrap().unwrap().text;
        assert!(got.contains("version = \"1.16.0\""));
        assert!(
            got.contains(
                "six = [\n    {file = \"six-1.16.0-py2.py3-none-any.whl\", hash = \"sha256:bbbb"
            ),
            "{got}"
        );
        assert!(!got.contains("1.15.0"));
    }

    #[test]
    fn the_content_hash_follows_a_changed_pyproject() {
        let v1 = include_str!("../fixtures/lock/poetry1-pyproject.toml");
        let new = v1.replacen(
            "[tool.poetry.dependencies]",
            "[tool.poetry.dependencies]\nzzz-new = \"*\"",
            1,
        );
        assert_ne!(v1, new);
        let (from, to) = (facts("1.15.0", 'a'), facts("1.16.0", 'b'));
        let store = FactStore::new();
        let p = pin();
        let c = LockIn {
            pin: &p,
            from: &from,
            to: &to,
            requirement: None,
            manifest: Some((v1, &new)),
            store: &store,
        };
        let l = lock(false, "1.15.0", 'a').replace(
            "\"abc\"",
            "\"71646f3e3fae85c295e3852270969facafc502957e00610217e52c41c35fb270\"",
        );
        let got = rewrite(&l, &c).unwrap().unwrap().text;
        assert!(!got.contains("71646f3e"), "{got}");
        assert!(got.contains(&hashes::poetry_hashes(&new).unwrap()[0]));
        assert!(
            rewrite(&lock(false, "1.15.0", 'a'), &c)
                .unwrap_err()
                .contains("content-hash")
        );
    }

    #[test]
    fn poetry_constraints_become_pep_440() {
        assert_eq!(to_pep440("^1.2").as_deref(), Some(">=1.2,<2"));
        assert_eq!(to_pep440("^0.2.3").as_deref(), Some(">=0.2.3,<0.3"));
        assert_eq!(to_pep440("~1.2.3").as_deref(), Some(">=1.2.3,<1.3"));
        assert_eq!(to_pep440("~1").as_deref(), Some(">=1,<2"));
        assert_eq!(to_pep440(">=2.8,<3").as_deref(), Some(">=2.8,<3"));
        assert_eq!(to_pep440("*").as_deref(), Some(""));
        assert_eq!(to_pep440("1.2.3").as_deref(), Some("==1.2.3"));
        assert_eq!(to_pep440("1.2.*").as_deref(), Some("==1.2.*"));
        assert_eq!(to_pep440(">=1 || <0.5"), None);
    }

    #[test]
    fn dependents_with_their_constraints_are_read() {
        let r = requesters(&lock(false, "1.15.0", 'a'), &pin()).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "other");
        assert_eq!(r[0].range.as_deref(), Some(">=1.5"));
    }
}
