//! `composer.lock`.
//!
//! A package entry of the lock is the package's own `composer.json` as the registry publishes it, plus
//! where to fetch it from. Moving a pin replaces the whole entry from the registry's metadata for the new
//! version (its `source`, `dist`, `autoload`, `require` and the rest), written the way Composer writes it,
//! and when the move also changed `composer.json` the lock's `content-hash` is recomputed, after first
//! showing that the recomputation gives the hash the lock has for the old file.

use serde_json::Value;

use super::{LockIn, Requester, changed};
use crate::deps::ordered::{Ordered, parse_ordered};
use crate::deps::rewrite::{
    Edits, Member, Outcome, Pin, find, members, root_members, skip_value, skip_ws,
};

fn packages_of(doc: &Value) -> impl Iterator<Item = &Value> {
    ["packages", "packages-dev"]
        .into_iter()
        .filter_map(|k| doc.get(k).and_then(Value::as_array))
        .flatten()
}

pub fn requesters(text: &str, pin: &Pin<'_>) -> Result<Vec<Requester>, String> {
    let doc: Value =
        serde_json::from_str(text).map_err(|_| "composer.lock is not valid JSON".to_owned())?;
    let mut out = Vec::new();
    for p in packages_of(&doc) {
        if let Some(r) = p
            .get("require")
            .and_then(|r| r.get(pin.name))
            .and_then(Value::as_str)
        {
            out.push(Requester {
                name: p
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                version: p
                    .get("version")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                range: Some(r.to_owned()),
            });
        }
    }
    Ok(out)
}

/// The keys of a lock entry, in the order Composer writes them (read off a real lock).
const ORDER: [&str; 26] = [
    "name",
    "version",
    "source",
    "dist",
    "require",
    "conflict",
    "replace",
    "provide",
    "require-dev",
    "suggest",
    "bin",
    "type",
    "extra",
    "autoload",
    "autoload-dev",
    "notification-url",
    "include-path",
    "php-ext",
    "license",
    "authors",
    "description",
    "homepage",
    "keywords",
    "support",
    "funding",
    "time",
];

/// The members of `source` and `dist`, in the order Composer writes them.
const FIXED: [&str; 4] = ["type", "url", "reference", "shasum"];

fn pad(n: usize) -> String {
    " ".repeat(n)
}

/// A string as `json_encode` with `JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE` writes it.
fn string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `v` as PHP's `json_encode` with `JSON_PRETTY_PRINT | JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE`
/// writes it, at `level` indents of four spaces. Members keep the order they have; `fixed` puts the
/// members of a `source` or `dist` in Composer's own.
pub fn pretty(v: &Ordered, level: usize, fixed: bool) -> String {
    match v {
        Ordered::Null => "null".into(),
        Ordered::Bool(b) => b.to_string(),
        Ordered::Int(n) | Ordered::Num(n) => n.clone(),
        Ordered::Str(s) => string(s),
        Ordered::Arr(a) if a.is_empty() => "[]".into(),
        Ordered::Obj(m) if m.is_empty() => "{}".into(),
        Ordered::Arr(a) => {
            let items: Vec<String> = a
                .iter()
                .map(|x| format!("{}{}", pad((level + 1) * 4), pretty(x, level + 1, false)))
                .collect();
            format!("[\n{}\n{}]", items.join(",\n"), pad(level * 4))
        }
        Ordered::Obj(m) => {
            let mut members: Vec<&(String, Ordered)> = m.iter().collect();
            if fixed {
                members
                    .sort_by_key(|(k, _)| FIXED.iter().position(|f| f == k).unwrap_or(usize::MAX));
            }
            let items: Vec<String> = members
                .into_iter()
                .map(|(k, x)| {
                    format!(
                        "{}{}: {}",
                        pad((level + 1) * 4),
                        string(k),
                        pretty(x, level + 1, matches!(k.as_str(), "source" | "dist"))
                    )
                })
                .collect();
            format!("{{\n{}\n{}}}", items.join(",\n"), pad(level * 4))
        }
    }
}

/// The entry the lock should hold for the new version: the registry's, restricted to the keys a lock
/// entry has and put in the order Composer writes them, with the `notification-url` the old entry had
/// (it comes from the repository the package was installed from, not from the package).
fn lock_entry(entry: &Ordered, old: Option<&Ordered>) -> Result<Ordered, String> {
    let Ordered::Obj(_) = entry else {
        return Err("the registry's entry is not an object".into());
    };
    let mut out = Vec::new();
    for k in ORDER {
        let v = entry.get(k).or_else(|| {
            old.and_then(|o| o.get(k))
                .filter(|_| k == "notification-url")
        });
        if let Some(v) = v {
            out.push((k.to_owned(), v.clone()));
        }
    }
    Ok(Ordered::Obj(out))
}

pub fn rewrite(text: &str, c: &LockIn<'_>) -> Outcome {
    let pin = c.pin;
    let entry =
        c.to.entry
            .as_ref()
            .ok_or("the registry's metadata for the new version is not known")?;
    let top = root_members(text)?;
    let mut edits = Edits::new();
    let mut found = 0usize;
    let b = text.as_bytes();
    for section in ["packages", "packages-dev"] {
        let Some(arr) = find(&top, section).filter(|m| m.is_arr()) else {
            continue;
        };
        let mut i = skip_ws(b, arr.val_start + 1);
        while i < b.len() && b[i] == b'{' {
            let ms = members(text, i).ok_or("composer.lock is malformed")?;
            let end = skip_value(b, i).ok_or("composer.lock is malformed")?;
            let name = member_str(text, &ms, "name");
            let version = member_str(text, &ms, "version");
            if name.eq_ignore_ascii_case(pin.name)
                && version.strip_prefix('v').unwrap_or(&version)
                    == pin.from.strip_prefix('v').unwrap_or(pin.from)
            {
                check_old(&ms, text, c)?;
                let old = parse_ordered(&text[i..end]).ok();
                let new_entry = lock_entry(entry, old.as_ref())?;
                let level = indent_level(text, i);
                edits.push(i, end, pretty(&new_entry, level, false));
                found += 1;
            }
            i = skip_ws(b, end);
            if b.get(i) == Some(&b',') {
                i = skip_ws(b, i + 1);
            }
        }
    }
    if found == 0 {
        return Ok(None);
    }
    let mut new_text = edits.apply(text);
    if let Some((old_json, new_json)) = c.manifest
        && old_json != new_json
    {
        new_text = super::hashes::set_composer_hash(&new_text, old_json, new_json)?;
    }
    changed(text, new_text)
}

fn member_str(text: &str, ms: &[Member], key: &str) -> String {
    find(ms, key)
        .filter(|m| m.is_str())
        .map(|m| m.string(text).to_owned())
        .unwrap_or_default()
}

/// The old entry's `dist` must be the registry's own for the old version.
fn check_old(ms: &[Member], text: &str, c: &LockIn<'_>) -> Result<(), String> {
    let Some(dist) = find(ms, "dist").filter(|d| d.is_obj()) else {
        return Ok(());
    };
    let dm = members(text, dist.val_start).unwrap_or_default();
    let (old_url, old_ref) = (
        member_str(text, &dm, "url"),
        member_str(text, &dm, "reference"),
    );
    if let Some(from) = c.from.entry.as_ref().and_then(|e| e.get("dist")) {
        let want_url = from.get("url").and_then(Ordered::as_str).unwrap_or("");
        let want_ref = from
            .get("reference")
            .and_then(Ordered::as_str)
            .unwrap_or("");
        if (old_url != want_url && !want_url.is_empty())
            || (old_ref != want_ref && !want_ref.is_empty())
        {
            return Err(format!(
                "the locked dist of {} {} is not the one the registry publishes",
                c.pin.name, c.pin.from
            ));
        }
    }
    Ok(())
}

/// How many four-space levels the line holding `at` is indented.
fn indent_level(text: &str, at: usize) -> usize {
    let line_start = text[..at].rfind('\n').map_or(0, |p| p + 1);
    (at - line_start) / 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::facts::{FactStore, Facts, parse_packagist_version};
    use crate::deps::model::Ecosystem;

    const PACKAGIST: &[u8] = include_bytes!("../fixtures/registry/packagist-psr-log.json");

    fn facts(v: &str) -> Facts {
        parse_packagist_version(PACKAGIST, "psr/log", v).unwrap()
    }

    fn lock_with(v: &str) -> String {
        let f = facts(v);
        let entry = lock_entry(f.entry.as_ref().unwrap(), None).unwrap();
        format!(
            "{{\n    \"_readme\": [\"x\"],\n    \"content-hash\": \"abc\",\n    \"packages\": [\n        {},\n        {{\n            \"name\": \"zzz/other\",\n            \"version\": \"1.0.0\",\n            \"require\": {{\n                \"psr/log\": \"^1.0 || ^2.0 || ^3.0\"\n            }}\n        }}\n    ],\n    \"packages-dev\": []\n}}\n",
            pretty(&entry, 2, false)
        )
    }

    fn pin() -> Pin<'static> {
        Pin {
            eco: Ecosystem::Packagist,
            name: "psr/log",
            from: "3.0.0",
            to: "3.0.2",
        }
    }

    #[test]
    fn the_entry_is_replaced_with_the_registrys_in_composers_layout() {
        let (from, to) = (facts("3.0.0"), facts("3.0.2"));
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
        let old = lock_with("3.0.0");
        let got = rewrite(&old, &c).unwrap().unwrap().text;
        assert_eq!(got, lock_with("3.0.2"));
        // Composer's layout: four-space indents, slashes as they are, fixed key order for source and dist.
        assert!(got.contains("        {\n            \"name\": \"psr/log\",\n            \"version\": \"3.0.2\",\n            \"source\": {\n                \"type\": \"git\",\n                \"url\": \"https://github.com/php-fig/log.git\",\n                \"reference\": "), "{got}");
        assert!(got.contains("\"dist\": {\n                \"type\": \"zip\",\n                \"url\": \"https://api.github.com/repos/php-fig/log/zipball/"));
        let v: Value = serde_json::from_str(&got).unwrap();
        assert_eq!(v["packages"][0]["version"], "3.0.2");
        assert_eq!(v["packages"][1]["name"], "zzz/other");
        assert_eq!(
            requesters(&got, &p).unwrap()[0].range.as_deref(),
            Some("^1.0 || ^2.0 || ^3.0")
        );
    }

    #[test]
    fn the_old_notification_url_is_kept() {
        let (from, to) = (facts("3.0.0"), facts("3.0.2"));
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
        let old = lock_with("3.0.0").replacen(
            "\"require\": {\n                \"php\"",
            "\"notification-url\": \"https://packagist.example/downloads/\",\n            \"require\": {\n                \"php\"",
            1,
        );
        assert!(old.contains("notification-url"));
        let got = rewrite(&old, &c).unwrap().unwrap().text;
        assert!(
            got.contains("\"notification-url\": \"https://packagist.example/downloads/\""),
            "{got}"
        );
    }

    #[test]
    fn a_lock_whose_dist_is_not_the_registrys_for_the_old_version_is_left_alone() {
        let (from, to) = (facts("3.0.0"), facts("3.0.2"));
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
        let forged = lock_with("3.0.0").replacen("zipball/", "zipball/0", 1);
        assert!(
            rewrite(&forged, &c)
                .unwrap_err()
                .contains("not the one the registry")
        );
    }

    #[test]
    fn the_content_hash_follows_a_changed_manifest_only_when_the_old_one_is_reproduced() {
        let json = include_str!("../fixtures/lock/composer-demo.json");
        let (from, to) = (facts("3.0.0"), facts("3.0.2"));
        let store = FactStore::new();
        let p = pin();
        let new_json = json.replacen("\"php\": \"", "\"php\": \">=0.1\", \"x-removed\": \"", 1);
        let c = LockIn {
            pin: &p,
            from: &from,
            to: &to,
            requirement: None,
            manifest: Some((json, &new_json)),
            store: &store,
        };
        let lock = lock_with("3.0.0").replace("\"abc\"", "\"7da9d2d6ad7c2c87d65f888053a4e4a1\"");
        let got = rewrite(&lock, &c).unwrap().unwrap().text;
        assert!(!got.contains("7da9d2d6"), "{got}");
        let bad = lock_with("3.0.0");
        assert!(rewrite(&bad, &c).unwrap_err().contains("content-hash"));
    }

    #[test]
    fn requirements_a_new_version_would_break_are_found_in_the_lock() {
        let r = requesters(&lock_with("3.0.0"), &pin()).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "zzz/other");
    }
}
