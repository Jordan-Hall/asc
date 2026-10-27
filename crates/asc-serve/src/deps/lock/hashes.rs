//! The hashes a lockfile holds of its manifest.
//!
//! `composer.lock` and `poetry.lock` record a `content-hash` of the parts of `composer.json` or
//! `pyproject.toml` the lock depends on. A move that edits the manifest changes it, and the lock is then
//! stale to the package manager unless the hash is written again. The algorithms are the tools' own and
//! not documented as stable, so a hash is only written after the same code has been shown to reproduce
//! the hash the lock holds for the *old* manifest; if it cannot, the lock is not touched.

use crate::deps::ordered::{Ordered, parse_ordered};

/// MD5 (RFC 1321), as Composer's `hash('md5', ...)`.
pub fn md5_hex(data: &[u8]) -> String {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64)
        .map(|i| ((f64::from(i + 1)).sin().abs() * 4_294_967_296.0) as u32)
        .collect();
    let (mut a0, mut b0, mut c0, mut d0) = (
        0x6745_2301u32,
        0xefcd_ab89u32,
        0x98ba_dcfeu32,
        0x1032_5476u32,
    );
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_le_bytes());
    for chunk in msg.chunks(64) {
        let m: Vec<u32> = chunk
            .chunks(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect();
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f2 = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f2.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    [a0, b0, c0, d0]
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ------------------------------------------------------------------------------ Composer

/// A string as PHP's `json_encode` with no flags writes it: slashes escaped, everything outside ASCII as
/// `\uXXXX`.
fn php_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '/' => out.push_str("\\/"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) < 0x80 => out.push(c),
            c => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{u:04x}"));
                }
            }
        }
    }
    out.push('"');
    out
}

/// `v` as PHP's `json_encode($v, 0)` writes the array it decoded to.
pub fn php_encode(v: &Ordered) -> String {
    match v {
        Ordered::Null => "null".into(),
        Ordered::Bool(b) => b.to_string(),
        Ordered::Int(i) | Ordered::Num(i) => i.clone(),
        Ordered::Str(s) => php_string(s),
        Ordered::Arr(a) => format!(
            "[{}]",
            a.iter().map(php_encode).collect::<Vec<_>>().join(",")
        ),
        Ordered::Obj(m) if m.is_empty() => "[]".into(),
        // PHP turns an object whose keys are 0, 1, 2... into a list.
        Ordered::Obj(m) if m.iter().enumerate().all(|(i, (k, _))| *k == i.to_string()) => {
            format!(
                "[{}]",
                m.iter()
                    .map(|(_, v)| php_encode(v))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        Ordered::Obj(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!("{}:{}", php_string(k), php_encode(v)))
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

/// The `content-hash` Composer computes of a `composer.json`.
pub fn composer_hash(json: &str) -> Result<String, String> {
    let doc = parse_ordered(json)?;
    if doc.has_float() {
        return Err("composer.json holds a number PHP would write differently".into());
    }
    let Ordered::Obj(top) = doc else {
        return Err("composer.json is not an object".into());
    };
    const KEYS: [&str; 11] = [
        "name",
        "version",
        "require",
        "require-dev",
        "conflict",
        "replace",
        "provide",
        "minimum-stability",
        "prefer-stable",
        "repositories",
        "extra",
    ];
    let mut rel: Vec<(String, Ordered)> = KEYS
        .iter()
        .filter_map(|k| top.iter().find(|(x, _)| x == k).cloned())
        .collect();
    if let Some((_, Ordered::Obj(cfg))) = top.iter().find(|(k, _)| k == "config")
        && let Some((_, p)) = cfg.iter().find(|(k, _)| k == "platform")
    {
        rel.push((
            "config".into(),
            Ordered::Obj(vec![("platform".into(), p.clone())]),
        ));
    }
    rel.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(md5_hex(php_encode(&Ordered::Obj(rel)).as_bytes()).to_string())
}

/// Writes the new `content-hash` into `lock`, after showing that the old one is reproduced.
pub fn set_composer_hash(lock: &str, old_json: &str, new_json: &str) -> Result<String, String> {
    let r = regex::Regex::new(r#"("content-hash":\s*")([0-9a-f]{32})(")"#)
        .map_err(|e| e.to_string())?;
    let c = r
        .captures(lock)
        .ok_or("composer.lock has no content-hash")?;
    let have = &c[2];
    if composer_hash(old_json)? != have {
        return Err(
            "the content-hash of composer.lock is not what Composer's algorithm gives for the old composer.json, so the new one cannot be derived"
                .into(),
        );
    }
    let new = composer_hash(new_json)?;
    let m = c.get(2).ok_or("no hash")?;
    let mut out = lock.to_owned();
    out.replace_range(m.range(), &new);
    Ok(out)
}

// ------------------------------------------------------------------------------ Poetry

fn py_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) < 0x7f => out.push(c),
            // Python's `ensure_ascii` writes DEL and everything above as \u escapes.
            c => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{u:04x}"));
                }
            }
        }
    }
    out.push('"');
    out
}

/// `json.dumps(v, sort_keys=True)` of a TOML value.
fn py_dumps(v: &toml::Value) -> Result<String, String> {
    Ok(match v {
        toml::Value::String(s) => py_string(s),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Float(_) | toml::Value::Datetime(_) => {
            return Err("pyproject.toml holds a value the hash cannot represent".into());
        }
        toml::Value::Array(a) => format!(
            "[{}]",
            a.iter()
                .map(py_dumps)
                .collect::<Result<Vec<_>, _>>()?
                .join(", ")
        ),
        toml::Value::Table(t) => {
            let mut keys: Vec<&String> = t.keys().collect();
            keys.sort();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|k| Ok(format!("{}: {}", py_string(k), py_dumps(&t[k])?)))
                    .collect::<Result<Vec<_>, String>>()?
                    .join(", ")
            )
        }
    })
}

/// The content hashes Poetry may have computed of a `pyproject.toml`: the 1.x one, and the 2.x one that
/// nests `[project]` and the dependency groups.
pub fn poetry_hashes(pyproject: &str) -> Result<[String; 2], String> {
    let doc: toml::Value =
        toml::from_str(pyproject).map_err(|_| "pyproject.toml is not valid TOML".to_owned())?;
    let poetry = doc
        .get("tool")
        .and_then(|t| t.get("poetry"))
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default();
    let project = doc
        .get("project")
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default();
    let groups = doc
        .get("dependency-groups")
        .and_then(toml::Value::as_table)
        .cloned();
    const LEGACY: [&str; 4] = ["dependencies", "source", "extras", "dev-dependencies"];
    let relevant = [
        "dependencies",
        "source",
        "extras",
        "dev-dependencies",
        "group",
    ];
    let null = toml::Value::String("\u{0}null".into());
    let build = |skip_none_legacy: bool| -> toml::value::Table {
        let mut t = toml::value::Table::new();
        for k in relevant {
            match poetry.get(k) {
                Some(v) => {
                    t.insert(k.to_owned(), v.clone());
                }
                None if LEGACY.contains(&k) && !skip_none_legacy => {
                    t.insert(k.to_owned(), null.clone());
                }
                None => {}
            }
        }
        t
    };
    let dump = |t: &toml::value::Table| -> Result<String, String> {
        // The placeholder stands for Python's None.
        Ok(py_dumps(&toml::Value::Table(t.clone()))?.replace("\"\\u0000null\"", "null"))
    };
    let sha = |s: &str| crate::deps::facts::sha256_hex(s.as_bytes());
    let v1 = sha(&dump(&build(false))?);
    let mut rp = toml::value::Table::new();
    for k in ["requires-python", "dependencies", "optional-dependencies"] {
        if let Some(v) = project.get(k) {
            rp.insert(k.to_owned(), v.clone());
        }
    }
    let v2 = if rp.is_empty() && groups.is_none() {
        v1.clone()
    } else {
        let mut top = toml::value::Table::new();
        top.insert("project".into(), toml::Value::Table(rp.clone()));
        let mut tool = toml::value::Table::new();
        tool.insert("poetry".into(), toml::Value::Table(build(!rp.is_empty())));
        top.insert("tool".into(), toml::Value::Table(tool));
        if let Some(g) = groups {
            top.insert("dependency-groups".into(), toml::Value::Table(g));
        }
        sha(&dump(&top)?)
    };
    Ok([v1, v2])
}

/// Writes the new `content-hash` into a `poetry.lock`, after showing that one of the algorithms
/// reproduces the old one.
pub fn set_poetry_hash(lock: &str, old_toml: &str, new_toml: &str) -> Result<String, String> {
    let r = regex::Regex::new(r#"(?m)^(content-hash = ")([0-9a-f]{64})(")"#)
        .map_err(|e| e.to_string())?;
    let c = r.captures(lock).ok_or("poetry.lock has no content-hash")?;
    let have = c[2].to_owned();
    let old = poetry_hashes(old_toml)?;
    let Some(which) = old.iter().position(|h| *h == have) else {
        return Err(
            "the content-hash of poetry.lock is not what any known Poetry version computes for the old pyproject.toml, so the new one cannot be derived"
                .into(),
        );
    };
    let new = &poetry_hashes(new_toml)?[which];
    let m = c.get(2).ok_or("no hash")?;
    let mut out = lock.to_owned();
    out.replace_range(m.range(), new);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_the_rfc_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            ),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    #[test]
    fn composer_reproduces_the_content_hash_of_a_real_lock() {
        // symfony/demo's composer.json and the content-hash of its composer.lock.
        let json = include_str!("../fixtures/lock/composer-demo.json");
        assert_eq!(
            composer_hash(json).unwrap(),
            "7da9d2d6ad7c2c87d65f888053a4e4a1"
        );
    }

    #[test]
    fn a_changed_requirement_changes_the_hash_and_only_a_reproducible_old_hash_is_replaced() {
        let json = include_str!("../fixtures/lock/composer-demo.json");
        let lock = "{\n    \"content-hash\": \"7da9d2d6ad7c2c87d65f888053a4e4a1\",\n    \"packages\": []\n}\n";
        let new_json = json.replace("\"php\": \">=8.", "\"php\": \">=9.");
        assert_ne!(json, new_json);
        let out = set_composer_hash(lock, json, &new_json).unwrap();
        assert!(out.contains(&composer_hash(&new_json).unwrap()));
        assert!(!out.contains("7da9d2d6"));
        // The lock was not made from this file.
        let e = set_composer_hash(lock, &new_json, json).unwrap_err();
        assert!(e.contains("cannot be derived"), "{e}");
    }

    #[test]
    fn php_encoding_escapes_slashes_and_unicode_and_turns_empty_objects_into_lists() {
        let v = parse_ordered(r#"{"b":{"a/b":"é😀"},"a":{},"c":{"0":1,"1":2},"d":[true,null,-3]}"#)
            .unwrap();
        assert_eq!(
            php_encode(&v),
            r#"{"b":{"a\/b":"\u00e9\ud83d\ude00"},"a":[],"c":[1,2],"d":[true,null,-3]}"#
        );
        assert!(parse_ordered("{\"a\": 1.5}").unwrap().has_float());
        assert!(composer_hash("{\"a\": 1.5}").is_err());
        assert!(parse_ordered("{\"a\": 1} x").is_err());
        assert!(parse_ordered(&"[".repeat(100)).is_err());
    }

    #[test]
    fn poetry_reproduces_both_generations_of_content_hash() {
        // Poetry 1.8.2 for poetry 1.8.3's own pyproject.toml, and Poetry 2.5.1 for its main branch's.
        let v1 = include_str!("../fixtures/lock/poetry1-pyproject.toml");
        let v2 = include_str!("../fixtures/lock/poetry2-pyproject.toml");
        assert_eq!(
            poetry_hashes(v1).unwrap()[0],
            "71646f3e3fae85c295e3852270969facafc502957e00610217e52c41c35fb270"
        );
        assert_eq!(
            poetry_hashes(v2).unwrap()[1],
            "d2229bfd6735858e0017668465dd83738f34358fb7fc44269ca67ae9fee86742"
        );
    }

    #[test]
    fn the_poetry_hash_is_replaced_by_the_algorithm_that_reproduced_the_old_one() {
        let v2 = include_str!("../fixtures/lock/poetry2-pyproject.toml");
        let lock = "[metadata]\nlock-version = \"2.1\"\npython-versions = \">=3.10\"\ncontent-hash = \"d2229bfd6735858e0017668465dd83738f34358fb7fc44269ca67ae9fee86742\"\n";
        let new = v2.replacen("requires-python", "requires-python = \">=3.12\"\n#", 1);
        let out = set_poetry_hash(lock, v2, &new).unwrap();
        assert!(!out.contains("d2229bfd"));
        let h = poetry_hashes(&new).unwrap();
        assert!(out.contains(&h[1]));
        let e = set_poetry_hash(lock, &new, v2).unwrap_err();
        assert!(e.contains("cannot be derived"), "{e}");
    }
}
