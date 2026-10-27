//! `pnpm-lock.yaml` (lockfile versions 6 and 9).
//!
//! The package is named in four places: the importers (`specifier` and `version` of the project that asks
//! for it), the `packages` entry that holds the tarball's integrity, the `snapshots` entry (version 9) or
//! the same `packages` entry (version 6) that lists what it depends on, and every dependency map and peer
//! suffix (`react-dom@18.2.0(react@18.2.0)`) that mentions the version. Moving a pin renames all of them
//! and writes the new integrity; the entries stay in pnpm's sorted order.

use super::{LockIn, Requester, changed};
use crate::deps::facts::integrity_consistent;
use crate::deps::model::Ecosystem;
use crate::deps::ranges::admits;
use crate::deps::rewrite::{Outcome, Pin};

fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// Where a `key: value` line splits: the first `:` followed by a space or the end.
fn colon(line: &str) -> Option<usize> {
    let b = line.as_bytes();
    (0..b.len()).find(|&i| b[i] == b':' && (i + 1 == b.len() || b[i + 1] == b' '))
}

fn unquote(s: &str) -> &str {
    s.trim().trim_matches(['\'', '"'])
}

/// `(name, version, suffix)` of a `packages`/`snapshots` key such as `/@scope/name@1.2.3(peer@1)`.
fn split_key(key: &str) -> Option<(String, String, String)> {
    let key = key.strip_prefix('/').unwrap_or(key);
    let at = key.get(1..)?.find('@')? + 1;
    let (name, rest) = (&key[..at], &key[at + 1..]);
    let cut = rest.find('(').unwrap_or(rest.len());
    Some((
        name.to_owned(),
        rest[..cut].to_owned(),
        rest[cut..].to_owned(),
    ))
}

fn lockfile_major(text: &str) -> Result<u32, String> {
    let line = text
        .lines()
        .find(|l| l.starts_with("lockfileVersion:"))
        .ok_or("pnpm-lock.yaml has no lockfileVersion")?;
    let v = unquote(line.trim_start_matches("lockfileVersion:"));
    v.split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .ok_or_else(|| "the lockfileVersion of pnpm-lock.yaml is not a number".to_owned())
}

fn check_version(text: &str) -> Result<(), String> {
    if text.contains('\r') {
        return Err("pnpm-lock.yaml has Windows line endings".into());
    }
    let major = lockfile_major(text)?;
    if major < 6 {
        return Err(format!(
            "lockfileVersion {major} (pnpm 7 and older) is not rewritten; only 6 and 9 are"
        ));
    }
    if major > 9 {
        return Err(format!(
            "lockfileVersion {major} is newer than this code knows"
        ));
    }
    Ok(())
}

pub fn requesters(text: &str, pin: &Pin<'_>) -> Result<Vec<Requester>, String> {
    check_version(text)?;
    let mut out = Vec::new();
    let mut section = "";
    let mut entry: Option<(String, String)> = None;
    let mut block = false;
    for line in text.lines() {
        let ind = indent(line);
        if ind == 0 && !line.is_empty() {
            section = line.split(':').next().unwrap_or("");
            entry = None;
            block = false;
            continue;
        }
        if !matches!(section, "packages" | "snapshots") {
            continue;
        }
        if ind == 2
            && let Some(i) = colon(line)
        {
            entry = split_key(unquote(&line[2..i])).map(|(n, v, _)| (n, v));
            block = false;
        } else if ind == 4 {
            block = matches!(line.trim(), "dependencies:" | "optionalDependencies:");
        } else if ind == 6
            && block
            && let Some(i) = colon(line)
            && unquote(&line[6..i]) == pin.name
            && let Some((n, v)) = &entry
        {
            // The dependency's own version is what the line says; the requester's range is not in the lock.
            out.push(Requester {
                name: n.clone(),
                version: v.clone(),
                range: None,
            });
        }
    }
    out.dedup();
    Ok(out)
}

/// `value` with the leading version `from` replaced by `to`, when it is that version.
fn bump_value(value: &str, from: &str, to: &str) -> Option<String> {
    let rest = value.strip_prefix(from)?;
    (rest.is_empty() || rest.starts_with('(')).then(|| format!("{to}{rest}"))
}

/// Peer suffixes that name the pin: `(name@from)` and `(name@from(`.
fn rename_suffixes(line: &str, pin: &Pin<'_>) -> String {
    let a = format!("({}@{})", pin.name, pin.from);
    let b = format!("({}@{}(", pin.name, pin.from);
    line.replace(&a, &format!("({}@{})", pin.name, pin.to))
        .replace(&b, &format!("({}@{}(", pin.name, pin.to))
}

/// The key of a block, for sorting.
fn block_key(block: &[String]) -> String {
    let line = &block[0];
    colon(line).map_or(String::new(), |i| unquote(&line[2..i]).to_owned())
}

/// Splits the lines of a section body into blocks that start at an indent-2 key.
fn blocks(body: &[String]) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    for l in body {
        if indent(l) == 2 && !l.trim().is_empty() {
            out.push(vec![l.clone()]);
        } else if let Some(b) = out.last_mut() {
            b.push(l.clone());
        } else {
            out.push(vec![l.clone()]);
        }
    }
    out
}

pub fn rewrite(text: &str, c: &LockIn<'_>) -> Outcome {
    check_version(text)?;
    let pin = c.pin;
    let old: Vec<&str> = text.split('\n').collect();
    let mut lines: Vec<String> = Vec::with_capacity(old.len());
    let mut section = String::new();
    let mut in_target = false;
    let mut block = false;
    let mut imp_dep = false;
    let mut touched = false;
    let mut renamed_key = false;
    for (idx, raw) in old.iter().enumerate() {
        let mut line = (*raw).to_owned();
        let ind = indent(&line);
        if ind == 0 && !line.is_empty() {
            section = line.split(':').next().unwrap_or("").to_owned();
            in_target = false;
            block = false;
            imp_dep = false;
            lines.push(line);
            continue;
        }
        match section.as_str() {
            "packages" | "snapshots" => {
                if ind == 2
                    && let Some(i) = colon(&line)
                {
                    in_target = false;
                    block = false;
                    let key = unquote(&line[2..i]).to_owned();
                    if let Some((n, v, suffix)) = split_key(&key)
                        && n == pin.name
                    {
                        if v == pin.from {
                            let slash = if key.starts_with('/') { "/" } else { "" };
                            let new_key =
                                format!("{slash}{n}@{}{}", pin.to, rename_suffixes(&suffix, pin));
                            let quote = line[2..].starts_with(['\'', '"']);
                            let key_text = if quote {
                                format!("'{new_key}'")
                            } else {
                                new_key
                            };
                            // The rest of the line after the key: the `:` and any value.
                            line = format!("  {key_text}{}", &raw[i..]);
                            in_target = section == "packages" && suffix.is_empty();
                            renamed_key = true;
                            touched = true;
                        } else if v == pin.to {
                            return Err(format!(
                                "pnpm-lock.yaml already locks {} {}; pnpm would merge the entries",
                                pin.name, pin.to
                            ));
                        }
                    } else {
                        let renamed = rename_suffixes(&line, pin);
                        if renamed != line {
                            line = renamed;
                            touched = true;
                        }
                    }
                } else if ind == 4 {
                    block = matches!(line.trim(), "dependencies:" | "optionalDependencies:");
                    if in_target && line.trim_start().starts_with("resolution:") {
                        line = resolution_line(&line, c)?;
                        touched = true;
                    }
                } else if ind == 6
                    && block
                    && let Some(i) = colon(&line)
                {
                    let name = unquote(&line[6..i]).to_owned();
                    let value = line[i + 1..].trim().to_owned();
                    if name == pin.name
                        && let Some(v) = bump_value(unquote(&value), pin.from, pin.to)
                    {
                        let q = if value.starts_with(['\'', '"']) {
                            "'"
                        } else {
                            ""
                        };
                        line = format!("      {}: {q}{}{q}", &line[6..i], rename_suffixes(&v, pin));
                        touched = true;
                    } else {
                        let renamed = rename_suffixes(&line, pin);
                        if renamed != line {
                            line = renamed;
                            touched = true;
                        }
                    }
                }
            }
            "importers" => {
                if ind == 6
                    && let Some(i) = colon(&line)
                {
                    imp_dep = unquote(&line[6..i]) == pin.name
                        && line[i + 1..].trim().is_empty()
                        && importer_version(&old, idx)
                            .is_some_and(|v| bump_value(&v, pin.from, pin.to).is_some());
                } else if ind == 8 && imp_dep {
                    let t = line.trim_start();
                    if let Some(v) = t.strip_prefix("version:") {
                        let value = v.trim();
                        if let Some(nv) = bump_value(unquote(value), pin.from, pin.to) {
                            let q = if value.starts_with(['\'', '"']) {
                                "'"
                            } else {
                                ""
                            };
                            line = format!(
                                "{}version: {q}{}{q}",
                                " ".repeat(ind),
                                rename_suffixes(&nv, pin)
                            );
                            touched = true;
                        }
                    } else if let Some(s) = t.strip_prefix("specifier:") {
                        let spec = unquote(s.trim()).to_owned();
                        if admits(Ecosystem::Npm, &spec, pin.to) == Some(false)
                            && admits(Ecosystem::Npm, &spec, pin.from) == Some(true)
                        {
                            let req = c.requirement.ok_or_else(|| {
                                format!(
                                    "the specifier {spec} of {} does not admit {}, and the manifest did not change",
                                    pin.name, pin.to
                                )
                            })?;
                            let q = if needs_quotes(req) { "'" } else { "" };
                            line = format!("{}specifier: {q}{req}{q}", " ".repeat(ind));
                            touched = true;
                        } else if admits(Ecosystem::Npm, &spec, pin.to) != Some(true) {
                            return Err(format!(
                                "the specifier {spec} of {} is not one this code can move to {}",
                                pin.name, pin.to
                            ));
                        }
                    }
                } else if ind <= 6 {
                    imp_dep = false;
                }
            }
            _ => {}
        }
        lines.push(line);
    }
    if !touched {
        return Ok(None);
    }
    if !renamed_key {
        return Err(format!(
            "pnpm-lock.yaml mentions {} {} but has no entry for it",
            pin.name, pin.from
        ));
    }
    let lines = resort(&old, lines);
    changed(text, lines.join("\n"))
}

/// The `version:` of the importer dependency whose name is on line `at`.
fn importer_version(old: &[&str], at: usize) -> Option<String> {
    old[at + 1..]
        .iter()
        .take_while(|l| indent(l) > 6)
        .find_map(|l| {
            l.trim_start()
                .strip_prefix("version:")
                .map(|v| unquote(v).to_owned())
        })
}

fn needs_quotes(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c, ' ' | ':' | '#' | '*' | '|' | '>' | '<' | '{' | '[' | ','))
}

/// `resolution: {integrity: ...}` with the new integrity.
fn resolution_line(line: &str, c: &LockIn<'_>) -> Result<String, String> {
    let pin = c.pin;
    let t = line.trim_start();
    let body = t
        .strip_prefix("resolution:")
        .map(str::trim)
        .and_then(|b| b.strip_prefix('{'))
        .and_then(|b| b.strip_suffix('}'))
        .ok_or("a resolution of pnpm-lock.yaml is not a flow mapping")?;
    let parts: Vec<&str> = body.split(',').map(str::trim).collect();
    let [only] = parts.as_slice() else {
        return Err(format!(
            "the resolution of {} {} is not a registry tarball",
            pin.name, pin.from
        ));
    };
    let old = only
        .strip_prefix("integrity:")
        .map(|v| unquote(v).to_owned())
        .ok_or_else(|| {
            format!(
                "the resolution of {} {} is not a registry tarball",
                pin.name, pin.from
            )
        })?;
    if !integrity_consistent(&old, c.from.integrity.as_deref(), c.from.shasum.as_deref()) {
        return Err(format!(
            "the locked integrity of {} {} is not the one the registry publishes",
            pin.name, pin.from
        ));
    }
    let new =
        c.to.integrity
            .as_deref()
            .ok_or("the registry's integrity is not known")?;
    if crate::deps::facts::sri(new).is_none() {
        return Err("the registry's integrity is not shaNNN-base64".into());
    }
    Ok(format!(
        "{}resolution: {{integrity: {new}}}",
        " ".repeat(indent(line))
    ))
}

/// Puts the entries of `packages` and `snapshots` back in key order when they were in it.
fn resort(old: &[&str], new: Vec<String>) -> Vec<String> {
    let mut lines = new;
    for name in ["packages:", "snapshots:"] {
        let Some(start) = lines.iter().position(|l| l == name) else {
            continue;
        };
        let end = lines[start + 1..]
            .iter()
            .position(|l| indent(l) == 0 && !l.is_empty())
            .map_or(lines.len(), |p| start + 1 + p);
        let body: Vec<String> = lines[start + 1..end].to_vec();
        let old_body: Vec<String> = old
            .iter()
            .skip(start + 1)
            .take(end - start - 1)
            .map(|s| (*s).to_owned())
            .collect();
        let was = blocks(&old_body);
        let was_sorted = was
            .iter()
            .filter(|b| indent(&b[0]) == 2)
            .map(|b| block_key(b))
            .collect::<Vec<_>>()
            .windows(2)
            .all(|w| w[0] <= w[1]);
        if !was_sorted {
            continue;
        }
        let mut bs = blocks(&body);
        let lead: Vec<Vec<String>> = bs
            .iter()
            .take_while(|b| indent(&b[0]) != 2)
            .cloned()
            .collect();
        bs.drain(..lead.len());
        let tail_blank = bs
            .last()
            .map_or(0, |b| b.iter().rev().take_while(|l| l.is_empty()).count());
        bs.sort_by_key(|b| block_key(b));
        let n = bs.len();
        let mut out: Vec<String> = lead.into_iter().flatten().collect();
        for (i, mut b) in bs.into_iter().enumerate() {
            while b.last().is_some_and(String::is_empty) {
                b.pop();
            }
            out.extend(b);
            if i + 1 < n {
                out.push(String::new());
            } else {
                out.extend(std::iter::repeat_n(String::new(), tail_blank));
            }
        }
        lines.splice(start + 1..end, out);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::facts::FactStore;
    use crate::deps::facts::Facts;

    const INT_A: &str = "sha512-AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQ==";
    const INT_B: &str = "sha512-AgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAg==";

    fn lock9() -> String {
        format!(
            "lockfileVersion: '9.0'\n\nsettings:\n  autoInstallPeers: true\n\nimporters:\n\n  .:\n    dependencies:\n      lodash:\n        specifier: ^4.17.20\n        version: 4.17.20\n      react-dom:\n        specifier: ^18.2.0\n        version: 18.2.0(react@18.2.0)\n\npackages:\n\n  lodash@4.17.20:\n    resolution: {{integrity: {INT_A}}}\n\n  react-dom@18.2.0:\n    resolution: {{integrity: sha512-cccc}}\n    peerDependencies:\n      react: ^18.2.0\n\n  react@18.2.0:\n    resolution: {{integrity: sha512-dddd}}\n\nsnapshots:\n\n  lodash@4.17.20: {{}}\n\n  react-dom@18.2.0(react@18.2.0):\n    dependencies:\n      lodash: 4.17.20\n      react: 18.2.0\n\n  react@18.2.0:\n    dependencies:\n      lodash: 4.17.20\n"
        )
    }

    fn facts(version: &str, int: &str) -> Facts {
        Facts {
            integrity: Some(int.to_owned()),
            tarball: Some(format!("https://r.example/lodash-{version}.tgz")),
            ..Facts::default()
        }
    }

    fn pin(to: &'static str) -> Pin<'static> {
        Pin {
            eco: Ecosystem::Npm,
            name: "lodash",
            from: "4.17.20",
            to,
        }
    }

    #[test]
    fn every_mention_is_renamed_and_the_integrity_is_the_registrys() {
        let (from, to) = (facts("4.17.20", INT_A), facts("4.17.21", INT_B));
        let store = FactStore::new();
        let p = pin("4.17.21");
        let c = LockIn {
            pin: &p,
            from: &from,
            to: &to,
            requirement: None,
            manifest: None,
            store: &store,
        };
        let got = rewrite(&lock9(), &c).unwrap().unwrap().text;
        let want = lock9()
            .replace("version: 4.17.20", "version: 4.17.21")
            .replace("lodash@4.17.20", "lodash@4.17.21")
            .replace("lodash: 4.17.20", "lodash: 4.17.21")
            .replace(INT_A, INT_B);
        assert_eq!(got, want);
    }

    #[test]
    fn a_new_requirement_replaces_the_specifier_that_no_longer_admits() {
        let (from, to) = (facts("4.17.20", INT_A), facts("5.0.0", INT_B));
        let store = FactStore::new();
        let p = pin("5.0.0");
        let c = LockIn {
            pin: &p,
            from: &from,
            to: &to,
            requirement: Some("^5.0.0"),
            manifest: None,
            store: &store,
        };
        let got = rewrite(&lock9(), &c).unwrap().unwrap().text;
        assert!(
            got.contains("specifier: ^5.0.0\n        version: 5.0.0\n"),
            "{got}"
        );
        let c2 = LockIn {
            requirement: None,
            ..c
        };
        assert!(
            rewrite(&lock9(), &c2)
                .unwrap_err()
                .contains("does not admit")
        );
    }

    #[test]
    fn a_peer_suffix_that_names_the_pin_follows_it_and_entries_stay_sorted() {
        let t = "lockfileVersion: '9.0'\n\npackages:\n\n  a@1.0.0:\n    resolution: {integrity: sha512-aa}\n\n  react@18.2.0:\n    resolution: {integrity: sha512-bb}\n\nsnapshots:\n\n  a@1.0.0(react@18.2.0):\n    dependencies:\n      react: 18.2.0\n\n  react@18.2.0: {}\n";
        let (mut from, mut to) = (facts("18.2.0", INT_A), facts("18.3.1", INT_B));
        from.tarball = Some("https://r.example/react-18.2.0.tgz".into());
        to.tarball = Some("https://r.example/react-18.3.1.tgz".into());
        let store = FactStore::new();
        let p = Pin {
            eco: Ecosystem::Npm,
            name: "react",
            from: "18.2.0",
            to: "18.3.1",
        };
        let c = LockIn {
            pin: &p,
            from: &from,
            to: &to,
            requirement: None,
            manifest: None,
            store: &store,
        };
        let got = rewrite(t, &c);
        // The old integrity in the file is not the registry's for `from`.
        assert!(got.unwrap_err().contains("integrity"));
        let t2 = t.replace("sha512-bb", INT_A);
        let got = rewrite(&t2, &c).unwrap().unwrap().text;
        assert!(
            got.contains("  a@1.0.0(react@18.3.1):\n    dependencies:\n      react: 18.3.1\n"),
            "{got}"
        );
        assert!(got.contains("  react@18.3.1: {}"), "{got}");
        assert!(got.contains(&format!("resolution: {{integrity: {INT_B}}}")));
    }

    #[test]
    fn requesters_are_the_entries_that_list_the_pin_and_old_lockfiles_are_refused() {
        let r = requesters(&lock9(), &pin("4.17.21")).unwrap();
        let names: Vec<String> = r
            .iter()
            .map(|r| format!("{} {}", r.name, r.version))
            .collect();
        assert_eq!(names, ["react-dom 18.2.0", "react 18.2.0"]);
        let old = "lockfileVersion: 5.4\n\nspecifiers:\n  lodash: ^4.17.20\n";
        assert!(
            requesters(old, &pin("4.17.21"))
                .unwrap_err()
                .contains("pnpm 7")
        );
    }

    #[test]
    fn a_tarball_or_directory_resolution_is_not_a_registry_package() {
        let t = lock9().replace(
            &format!("{{integrity: {INT_A}}}"),
            "{integrity: sha512-x, tarball: https://x/y.tgz}",
        );
        let (from, to) = (facts("4.17.20", INT_A), facts("4.17.21", INT_B));
        let store = FactStore::new();
        let p = pin("4.17.21");
        let c = LockIn {
            pin: &p,
            from: &from,
            to: &to,
            requirement: None,
            manifest: None,
            store: &store,
        };
        assert!(
            rewrite(&t, &c)
                .unwrap_err()
                .contains("not a registry tarball")
        );
    }
}
