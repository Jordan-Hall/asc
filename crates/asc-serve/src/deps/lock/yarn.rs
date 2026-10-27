//! `yarn.lock` version 1.
//!
//! An entry is a header of the requested ranges (`lodash@^4.17.15, lodash@~4.17.0:`) and a body of
//! `version`, `resolved` (the tarball URL with the SHA-1 as its fragment), `integrity` and the
//! dependencies. Moving a pin rewrites the body from the registry, keeps the ranges the new version still
//! satisfies, replaces the one range the manifest's requirement replaced, and puts the entry back where
//! yarn would sort it. Yarn 2 and later (Berry) keep a checksum of an archive yarn builds itself, which no
//! registry publishes: that format is refused.

use super::{LockIn, Requester, changed};
use crate::deps::facts::{integrity_consistent, rebase_tarball};
use crate::deps::model::Ecosystem;
use crate::deps::ranges::admits;
use crate::deps::rewrite::{Outcome, Pin};

/// One entry: its text, the requested patterns and the fields of its body.
struct Entry {
    text: String,
    keys: Vec<String>,
}

impl Entry {
    fn line(&self, field: &str) -> Option<String> {
        self.text
            .lines()
            .skip(1)
            .find_map(|l| l.strip_prefix(&format!("  {field} ")).map(str::to_owned))
    }

    fn version(&self) -> Option<String> {
        self.line("version").map(|v| unquote(&v))
    }

    /// The package the entry is for, which is not the name in the key when the key is an alias.
    fn package(&self) -> Option<String> {
        let (name, range) = split_key(self.keys.first()?)?;
        Some(match range.strip_prefix("npm:") {
            Some(real) => split_key(real).map_or(real.to_owned(), |(n, _)| n),
            None => name,
        })
    }

    /// `(name, range)` of the dependencies the body lists.
    fn dependencies(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut inside = false;
        for l in self.text.lines().skip(1) {
            if l.starts_with("  dependencies:") || l.starts_with("  optionalDependencies:") {
                inside = true;
                continue;
            }
            if inside {
                if let Some(rest) = l.strip_prefix("    ") {
                    if let Some((n, r)) = split_dep(rest) {
                        out.push((n, r));
                    }
                } else {
                    inside = false;
                }
            }
        }
        out
    }
}

/// `s` as the inside of a yarn string (JSON-style escapes).
fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches('"').to_owned()
}

/// `name` and `range` of `name@range`; a scope's `@` does not split.
fn split_key(key: &str) -> Option<(String, String)> {
    let at = key.get(1..)?.find('@')? + 1;
    Some((key[..at].to_owned(), key[at + 1..].to_owned()))
}

fn split_dep(line: &str) -> Option<(String, String)> {
    let line = line.trim_end();
    let (name, range) = if let Some(rest) = line.strip_prefix('"') {
        let end = rest.find('"')?;
        (rest[..end].to_owned(), rest[end + 1..].trim_start())
    } else {
        let (n, r) = line.split_once(' ')?;
        (n.to_owned(), r.trim_start())
    };
    Some((name, unquote(range)))
}

/// The header's keys, unquoted.
fn parse_header(line: &str) -> Option<Vec<String>> {
    let body = line.strip_suffix(':')?;
    let mut keys = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in body.chars() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                keys.push(cur.trim().to_owned());
                cur.clear();
            }
            c => cur.push(c),
        }
    }
    keys.push(cur.trim().to_owned());
    (!quoted && keys.iter().all(|k| !k.is_empty())).then_some(keys)
}

/// Whether yarn writes the key in quotes.
fn needs_quotes(key: &str) -> bool {
    key.starts_with("true")
        || key.starts_with("false")
        || key
            .chars()
            .any(|c| matches!(c, ':' | '\\' | '"' | ',' | '[' | ']') || c.is_whitespace())
        || key.starts_with(|c: char| c.is_ascii_digit())
        || !key.starts_with(|c: char| c.is_ascii_alphabetic())
}

fn header_of(keys: &[String]) -> String {
    let parts: Vec<String> = keys
        .iter()
        .map(|k| {
            if needs_quotes(k) {
                format!("\"{k}\"")
            } else {
                k.clone()
            }
        })
        .collect();
    format!("{}:", parts.join(", "))
}

/// The comment lines and the entries of a v1 lockfile.
fn parse(text: &str) -> Result<(String, Vec<Entry>), String> {
    if text.contains("\n__metadata:") || text.starts_with("__metadata:") {
        return Err(
            "this is a Yarn 2+ (Berry) lockfile: its checksum is of an archive yarn builds itself, which no registry publishes, so it cannot be computed here"
                .into(),
        );
    }
    let mut lead = String::new();
    let mut entries: Vec<Entry> = Vec::new();
    for line in text.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\n', '\r']);
        let is_header = !bare.starts_with([' ', '#']) && bare.ends_with(':') && !bare.is_empty();
        if is_header {
            let keys = parse_header(bare).ok_or("a header of yarn.lock is not readable")?;
            entries.push(Entry {
                text: line.to_owned(),
                keys,
            });
        } else if let Some(e) = entries.last_mut() {
            e.text.push_str(line);
        } else {
            lead.push_str(line);
        }
    }
    if entries.is_empty() {
        return Err("yarn.lock has no entries".into());
    }
    Ok((lead, entries))
}

pub fn requesters(text: &str, pin: &Pin<'_>) -> Result<Vec<Requester>, String> {
    let (_, entries) = parse(text)?;
    let mut out = Vec::new();
    for e in &entries {
        for (n, r) in e.dependencies() {
            if n == pin.name {
                out.push(Requester {
                    name: e.package().unwrap_or_default(),
                    version: e.version().unwrap_or_default(),
                    range: Some(r),
                });
            }
        }
    }
    Ok(out)
}

fn joined(keys: &[String]) -> String {
    keys.join(", ")
}

pub fn rewrite(text: &str, c: &LockIn<'_>) -> Outcome {
    let pin = c.pin;
    let (lead, mut entries) = parse(text)?;
    let sorted = entries
        .windows(2)
        .all(|w| joined(&w[0].keys) <= joined(&w[1].keys));
    let target = |e: &Entry| e.package().as_deref() == Some(pin.name);
    let mut moved = Vec::new();
    let mut replaced: Vec<(usize, Entry)> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        if !target(e) || e.version().as_deref() != Some(pin.from) {
            if target(e) && e.version().as_deref() == Some(pin.to) {
                return Err(format!(
                    "another entry of yarn.lock already locks {} {}; yarn would merge them",
                    pin.name, pin.to
                ));
            }
            continue;
        }
        let mut keys: Vec<String> = Vec::new();
        for k in &e.keys {
            let (name, range) = split_key(k).ok_or("a key of yarn.lock is not name@range")?;
            if range.starts_with("npm:") {
                return Err(format!(
                    "{k} is an alias, whose requirement is not one this code rewrites"
                ));
            }
            let new = match admits(Ecosystem::Npm, &range, pin.to) {
                Some(true) => range,
                Some(false) => match c.requirement {
                    Some(r) if admits(Ecosystem::Npm, &range, pin.from) == Some(true) => {
                        r.to_owned()
                    }
                    _ => {
                        return Err(format!(
                            "{k} is requested by something whose requirement the new version does not satisfy"
                        ));
                    }
                },
                None => {
                    return Err(format!(
                        "the range of {k} is not one this code can evaluate"
                    ));
                }
            };
            let nk = format!("{name}@{new}");
            if !keys.contains(&nk) {
                keys.push(nk);
            }
        }
        keys.sort();
        let body = rewrite_body(e, c)?;
        let text = format!("{}\n{body}", header_of(&keys));
        replaced.push((i, Entry { text, keys }));
    }
    for (i, e) in replaced {
        entries[i] = e;
        moved.push(i);
    }
    if moved.is_empty() {
        return Ok(None);
    }
    // Entries whose header changed go where yarn sorts them, when the file was sorted to begin with.
    if sorted {
        let mut taken = Vec::new();
        for &i in moved.iter().rev() {
            taken.push(entries.remove(i));
        }
        for e in taken {
            let at = entries.partition_point(|x| joined(&x.keys) < joined(&e.keys));
            entries.insert(at, e);
        }
    }
    let mut out = lead;
    let n = entries.len();
    for (i, e) in entries.iter().enumerate() {
        let mut t = e.text.trim_end().to_owned();
        t.push('\n');
        if i + 1 < n {
            t.push('\n');
        }
        out.push_str(&t);
    }
    if text.ends_with("\n\n") {
        out.push('\n');
    }
    changed(text, out)
}

/// The body of `e` with the new version, tarball and integrity.
fn rewrite_body(e: &Entry, c: &LockIn<'_>) -> Result<String, String> {
    let pin = c.pin;
    let to_tar =
        c.to.tarball
            .as_deref()
            .ok_or("the registry's tarball is not known")?;
    let mut out = String::new();
    for l in e.text.lines().skip(1) {
        if let Some(v) = l.strip_prefix("  version ") {
            let _ = v;
            out.push_str(&format!("  version \"{}\"\n", pin.to));
        } else if let Some(r) = l.strip_prefix("  resolved ") {
            let old = unquote(r);
            let (base, frag) = old
                .split_once('#')
                .map_or((old.as_str(), ""), |(b, f)| (b, f));
            let from_tar = c
                .from
                .tarball
                .as_deref()
                .ok_or("the old tarball is not known")?;
            let new = rebase_tarball(base, from_tar, to_tar).ok_or_else(|| {
                format!(
                    "the locked tarball of {} {} is not the registry's own",
                    pin.name, pin.from
                )
            })?;
            if !frag.is_empty() {
                if let Some(sum) = &c.from.shasum
                    && frag != sum
                {
                    return Err(format!(
                        "the locked hash of {} {} is not the one the registry publishes",
                        pin.name, pin.from
                    ));
                }
                let sum =
                    c.to.shasum
                        .as_deref()
                        .ok_or("the registry publishes no SHA-1 for the new version")?;
                out.push_str(&format!(
                    "  resolved \"{}#{}\"\n",
                    quoted(&new),
                    quoted(sum)
                ));
            } else {
                out.push_str(&format!("  resolved \"{}\"\n", quoted(&new)));
            }
        } else if let Some(i) = l.strip_prefix("  integrity ") {
            if !integrity_consistent(
                i.trim(),
                c.from.integrity.as_deref(),
                c.from.shasum.as_deref(),
            ) {
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
            out.push_str(&format!("  integrity {new}\n"));
        } else {
            out.push_str(l);
            out.push('\n');
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::facts::FactStore;
    use crate::deps::facts::Facts;

    fn facts(version: &str, sha1: &str, integrity: &str) -> Facts {
        Facts {
            tarball: Some(format!(
                "https://registry.npmjs.org/lodash/-/lodash-{version}.tgz"
            )),
            shasum: Some(sha1.to_owned()),
            integrity: Some(integrity.to_owned()),
            ..Facts::default()
        }
    }

    const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const INT_A: &str = "sha512-AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQ==";
    const INT_B: &str = "sha512-AgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAg==";

    fn lock() -> String {
        format!(
            "# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.\n# yarn lockfile v1\n\n\n\"@babel/core@^7.0.0\":\n  version \"7.1.2\"\n  resolved \"https://registry.yarnpkg.com/@babel/core/-/core-7.1.2.tgz#{SHA_A}\"\n  dependencies:\n    lodash \"^4.17.0\"\n\nlodash@^4.17.0, lodash@^4.17.15:\n  version \"4.17.20\"\n  resolved \"https://registry.yarnpkg.com/lodash/-/lodash-4.17.20.tgz#{SHA_A}\"\n  integrity {INT_A}\n\nzzz@^1.0.0:\n  version \"1.0.0\"\n  resolved \"https://registry.yarnpkg.com/zzz/-/zzz-1.0.0.tgz#{SHA_A}\"\n"
        )
    }

    fn pin() -> Pin<'static> {
        Pin {
            eco: Ecosystem::Npm,
            name: "lodash",
            from: "4.17.20",
            to: "4.17.21",
        }
    }

    #[test]
    fn an_entry_is_rewritten_from_the_registry_and_nothing_else_changes() {
        let (from, to) = (
            facts("4.17.20", SHA_A, INT_A),
            facts("4.17.21", SHA_B, INT_B),
        );
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
        let got = rewrite(&lock(), &c).unwrap().unwrap().text;
        let want = lock()
            .replace("version \"4.17.20\"", "version \"4.17.21\"")
            .replace(
                &format!("lodash-4.17.20.tgz#{SHA_A}"),
                &format!("lodash-4.17.21.tgz#{SHA_B}"),
            )
            .replace(&format!("integrity {INT_A}"), &format!("integrity {INT_B}"));
        assert_eq!(got, want);
    }

    #[test]
    fn a_replaced_requirement_renames_the_key_and_the_entry_moves_to_its_sorted_place() {
        let (from, to) = (facts("4.17.20", SHA_A, INT_A), facts("5.0.0", SHA_B, INT_B));
        let store = FactStore::new();
        let p = Pin {
            to: "5.0.0",
            ..pin()
        };
        let c = LockIn {
            pin: &p,
            from: &from,
            to: &to,
            requirement: Some("^5.0.0"),
            manifest: None,
            store: &store,
        };
        // Both ranges are replaced by the one the manifest now has.
        let got = rewrite(&lock(), &c).unwrap().unwrap().text;
        assert!(
            got.contains("\nlodash@^5.0.0:\n  version \"5.0.0\"\n"),
            "{got}"
        );
        let order: Vec<&str> = got
            .lines()
            .filter(|l| !l.starts_with(' ') && l.ends_with(':'))
            .collect();
        assert_eq!(
            order,
            ["\"@babel/core@^7.0.0\":", "lodash@^5.0.0:", "zzz@^1.0.0:"]
        );
    }

    #[test]
    fn nothing_is_written_when_the_old_digest_is_not_the_registrys_or_the_host_is_foreign() {
        let (from, to) = (
            facts("4.17.20", SHA_B, INT_A),
            facts("4.17.21", SHA_B, INT_B),
        );
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
        assert!(rewrite(&lock(), &c).unwrap_err().contains("hash"));
        let from = facts("4.17.20", SHA_A, INT_B);
        let c = LockIn { from: &from, ..c };
        assert!(rewrite(&lock(), &c).unwrap_err().contains("integrity"));
        let from = facts("4.17.20", SHA_A, INT_A);
        let foreign = lock().replace(
            "registry.yarnpkg.com/lodash/-/lodash-4.17.20",
            "evil.example/other/lodash-4.17.20",
        );
        let c = LockIn { from: &from, ..c };
        assert!(
            rewrite(&foreign, &c)
                .unwrap_err()
                .contains("not the registry's own")
        );
    }

    #[test]
    fn berry_lockfiles_are_refused_with_the_reason() {
        let berry = "__metadata:\n  version: 6\n\n\"lodash@npm:^4.17.20\":\n  version: 4.17.20\n";
        let e = parse(berry).map(|_| ()).unwrap_err();
        assert!(e.contains("Berry") && e.contains("archive"), "{e}");
    }

    #[test]
    fn requesters_and_a_second_copy_are_found() {
        let r = requesters(&lock(), &pin()).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "@babel/core");
        assert_eq!(r[0].range.as_deref(), Some("^4.17.0"));
        let two = format!(
            "{}\nlodash@~4.17.21:\n  version \"4.17.21\"\n  resolved \"https://registry.yarnpkg.com/lodash/-/lodash-4.17.21.tgz#{SHA_B}\"\n",
            lock()
        );
        let (from, to) = (
            facts("4.17.20", SHA_A, INT_A),
            facts("4.17.21", SHA_B, INT_B),
        );
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
        assert!(rewrite(&two, &c).unwrap_err().contains("merge"));
    }

    #[test]
    fn quoting_follows_yarns_rule() {
        assert!(needs_quotes("@babel/core@^7.0.0"));
        assert!(needs_quotes("a@>=1 <2"));
        assert!(needs_quotes("a@1.2.3, b"));
        assert!(!needs_quotes("lodash@^4.17.15"));
        assert!(!needs_quotes("lodash@~4.17.0"));
        assert_eq!(
            header_of(&["a@^1".into(), "a@~1.2".into()]),
            "a@^1, a@~1.2:"
        );
    }
}
