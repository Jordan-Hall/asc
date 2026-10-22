//! `bun.lock` reader and the dependency closure of a workspace member
//! (SPEC §12A.4, "The bun.lock closure").
//!
//! [`parse`] keeps only what the closure needs, so the shared parse result is
//! much smaller than the 1 MiB file. A lockfile over a limit, or one that is
//! not a usable `bun.lock`, parses to an *unusable* value: the detector then
//! uses the lockfile's `FileId` as `external_fp` (SPEC §12A.10).

use std::collections::{BTreeSet, HashSet};

use crate::canon::Value;
use crate::jsonc;
use crate::model::{DepKind, Fingerprint, MAX_LOCKFILE_BYTES, MAX_LOCKFILE_PACKAGES, Parsed};

/// The four dependency tables of a `package.json` or a lock workspace entry.
pub const TABLES: [(&str, DepKind); 4] = [
    ("dependencies", DepKind::Normal),
    ("devDependencies", DepKind::Dev),
    ("peerDependencies", DepKind::Peer),
    ("optionalDependencies", DepKind::Optional),
];

/// Package tables whose names a resolved package requests.
const PACKAGE_TABLES: [&str; 3] = ["dependencies", "optionalDependencies", "peerDependencies"];

/// One requested dependency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DepRef<'a> {
    pub kind: DepKind,
    pub table: &'static str,
    pub name: &'a str,
    pub spec: &'a str,
}

/// The value under `key` of a canonical map.
pub fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    let Value::Map(m) = v else { return None };
    m.binary_search_by(|(k, _)| k.as_str().cmp(key))
        .ok()
        .map(|i| &m[i].1)
}

/// The text under `key` of a canonical map.
pub fn get_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    match get(v, key)? {
        Value::Str(s) => Some(s),
        _ => None,
    }
}

/// Every dependency of a package entry, table by table, in name order.
pub fn tables(entry: &Value) -> Vec<DepRef<'_>> {
    let mut out = Vec::new();
    for (table, kind) in TABLES {
        if let Some(Value::Map(m)) = get(entry, table) {
            for (name, spec) in m {
                if let Value::Str(spec) = spec {
                    out.push(DepRef {
                        kind,
                        table,
                        name,
                        spec,
                    });
                }
            }
        }
    }
    out
}

/// True when `bytes` open like a JSON document, not like a patch file.
pub fn looks_like_lock(bytes: &[u8]) -> bool {
    let rest = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    matches!(
        rest.iter().find(|b| !b.is_ascii_whitespace()),
        Some(b'{' | b'/')
    )
}

fn unusable(reason: impl Into<String>) -> Parsed {
    Parsed(Value::Map(vec![(
        "unusable".into(),
        Value::Str(reason.into()),
    )]))
}

/// Parses a `bun.lock` with the SPEC §12A.10 limits.
pub fn parse(bytes: &[u8]) -> Parsed {
    parse_with(bytes, MAX_LOCKFILE_BYTES, MAX_LOCKFILE_PACKAGES)
}

/// [`parse`] with explicit limits.
pub fn parse_with(bytes: &[u8], max_bytes: usize, max_packages: usize) -> Parsed {
    if bytes.len() > max_bytes {
        return unusable(format!("lockfile larger than {max_bytes} bytes"));
    }
    let doc = match jsonc::parse(bytes) {
        Ok(v) => v,
        Err(e) => return unusable(e.to_string()),
    };
    match reduce(&doc, max_packages) {
        Ok(v) => Parsed(v),
        Err(why) => unusable(why),
    }
}

fn sorted(pairs: Vec<(String, Value)>) -> Result<Value, String> {
    Value::map(pairs).map_err(|e| e.to_string())
}

/// The names of an object's keys, or `Err` when it is not an object.
fn string_map(v: &serde_json::Value) -> Result<Vec<(String, Value)>, String> {
    let o = v.as_object().ok_or("dependency table is not an object")?;
    o.iter()
        .map(|(k, s)| match s {
            serde_json::Value::String(s) => Ok((k.clone(), Value::Str(s.clone()))),
            _ => Err("dependency spec is not text".to_owned()),
        })
        .collect()
}

fn reduce(doc: &serde_json::Value, max_packages: usize) -> Result<Value, String> {
    let obj = doc.as_object().ok_or("not a JSON object")?;
    let mut workspaces = Vec::new();
    if let Some(w) = obj.get("workspaces") {
        for (key, e) in w.as_object().ok_or("workspaces is not an object")? {
            let e = e.as_object().ok_or("workspace entry is not an object")?;
            let mut pairs = Vec::new();
            if let Some(serde_json::Value::String(n)) = e.get("name") {
                pairs.push(("name".to_owned(), Value::Str(n.clone())));
            }
            for (table, _) in TABLES {
                if let Some(t) = e.get(table) {
                    pairs.push((table.to_owned(), sorted(string_map(t)?)?));
                }
            }
            workspaces.push((key.clone(), sorted(pairs)?));
        }
    }
    let mut packages = Vec::new();
    if let Some(p) = obj.get("packages") {
        let p = p.as_object().ok_or("packages is not an object")?;
        if p.len() > max_packages {
            return Err(format!("more than {max_packages} packages"));
        }
        for (key, e) in p {
            packages.push((key.clone(), package_entry(e)?));
        }
    }
    sorted(vec![
        ("packages".to_owned(), sorted(packages)?),
        ("workspaces".to_owned(), sorted(workspaces)?),
    ])
}

/// `[ident, integrity, [requested names]]` from a `packages` array.
fn package_entry(e: &serde_json::Value) -> Result<Value, String> {
    let a = e.as_array().ok_or("package entry is not an array")?;
    let ident = a
        .first()
        .and_then(|v| v.as_str())
        .ok_or("package entry has no identifier")?;
    let integrity = match a.last() {
        Some(serde_json::Value::String(s)) if a.len() >= 3 => s.as_str(),
        _ => "",
    };
    let mut names = BTreeSet::new();
    if let Some(meta) = a.iter().skip(1).find_map(|v| v.as_object()) {
        for table in PACKAGE_TABLES {
            if let Some(serde_json::Value::Object(o)) = meta.get(table) {
                names.extend(o.keys().cloned());
            }
        }
    }
    Ok(Value::Array(vec![
        Value::Str(ident.to_owned()),
        Value::Str(integrity.to_owned()),
        Value::Array(names.into_iter().map(Value::Str).collect()),
    ]))
}

/// True for the entry of a workspace member (`name@workspace:path`).
fn is_workspace(ident: &str) -> bool {
    ident.contains("@workspace:")
}

/// Key segments, with a scoped name (`@s/n`) counting as one.
fn segments(key: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut rest = key;
    while !rest.is_empty() {
        let first_len = if rest.starts_with('@') {
            rest.find('/').map(|i| {
                let tail = &rest[i + 1..];
                i + 1 + tail.find('/').unwrap_or(tail.len())
            })
        } else {
            rest.find('/')
        }
        .unwrap_or(rest.len());
        out.push(&key[start..start + first_len]);
        let skip = (first_len + 1).min(rest.len());
        start += skip;
        rest = &rest[skip..];
    }
    out
}

/// The keys to try for dependency `name` requested by `requester`: the
/// requester's own nested key, each shorter parent, then the name alone.
pub fn candidates(requester: &str, name: &str) -> Vec<String> {
    let segs = segments(requester);
    let mut out: Vec<String> = (1..=segs.len())
        .rev()
        .map(|n| format!("{}/{name}", segs[..n].join("/")))
        .collect();
    out.push(name.to_owned());
    out
}

/// A usable `bun.lock`, viewed in place.
#[derive(Clone, Copy, Debug)]
pub struct Lock<'a> {
    workspaces: &'a [(String, Value)],
    packages: &'a [(String, Value)],
}

impl<'a> Lock<'a> {
    /// The lock held by `parsed`, or the reason it cannot be used.
    pub fn view(parsed: &'a Parsed) -> Result<Self, &'a str> {
        if let Some(why) = get_str(&parsed.0, "unusable") {
            return Err(why);
        }
        match (get(&parsed.0, "workspaces"), get(&parsed.0, "packages")) {
            (Some(Value::Map(workspaces)), Some(Value::Map(packages))) => Ok(Self {
                workspaces,
                packages,
            }),
            _ => Err("not a bun.lock"),
        }
    }

    /// Workspace entries by their path key; `""` is the root.
    pub fn workspaces(&self) -> &'a [(String, Value)] {
        self.workspaces
    }

    pub fn workspace(&self, key: &str) -> Option<&'a Value> {
        let i = self
            .workspaces
            .binary_search_by(|(k, _)| k.as_str().cmp(key))
            .ok()?;
        Some(&self.workspaces[i].1)
    }

    fn package(&self, key: &str) -> Option<(&'a str, &'a Value)> {
        let i = self
            .packages
            .binary_search_by(|(k, _)| k.as_str().cmp(key))
            .ok()?;
        let (k, v) = &self.packages[i];
        Some((k, v))
    }

    fn resolve(&self, requester: &str, name: &str) -> Option<(&'a str, &'a Value)> {
        candidates(requester, name)
            .iter()
            .find_map(|c| self.package(c))
    }

    /// The resolved third-party closure of workspace entry `entry` as a
    /// fingerprint of sorted unique `("name@version", integrity, patch)`.
    ///
    /// `member` names the requester when the entry has no `name`; `patch`
    /// gives the content id of the patch file for a `name@version`.
    pub fn closure(
        &self,
        entry: &Value,
        member: &str,
        patch: &dyn Fn(&str) -> Option<[u8; 32]>,
    ) -> Fingerprint {
        let requester = get_str(entry, "name").unwrap_or(member);
        let mut seen: HashSet<&str> = HashSet::new();
        let mut stack: Vec<(&str, &Value)> = Vec::new();
        for d in tables(entry) {
            if let Some((k, e)) = self.resolve(requester, d.name)
                && seen.insert(k)
            {
                stack.push((k, e));
            }
        }
        let mut out: BTreeSet<[String; 3]> = BTreeSet::new();
        while let Some((key, e)) = stack.pop() {
            let Value::Array(a) = e else { continue };
            let [
                Value::Str(ident),
                Value::Str(integrity),
                Value::Array(names),
            ] = a.as_slice()
            else {
                continue;
            };
            if is_workspace(ident) {
                continue;
            }
            let patch_id = patch(ident).map(hex).unwrap_or_default();
            out.insert([ident.clone(), integrity.clone(), patch_id]);
            for n in names {
                let Value::Str(n) = n else { continue };
                if let Some((k, e)) = self.resolve(key, n)
                    && seen.insert(k)
                {
                    stack.push((k, e));
                }
            }
        }
        Value::Array(
            out.into_iter()
                .map(|t| Value::Array(t.into_iter().map(Value::Str).collect()))
                .collect(),
        )
        .fingerprint()
    }
}

fn hex(b: [u8; 32]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = r#"{
  "lockfileVersion": 1,
  // comments and trailing commas are fine
  "workspaces": {
    "": { "name": "root", "dependencies": { "react": "^18", }, },
    "apps/web": {
      "name": "web",
      "dependencies": { "react": "^19", "@asc/ui": "workspace:*", "left": "1" },
    },
    "packages/ui": { "name": "@asc/ui", "devDependencies": { "left": "1" } },
  },
  "packages": {
    "@asc/ui": ["@asc/ui@workspace:packages/ui"],
    "left": ["left@1.0.0", "https://r/left.tgz", {}, "sha512-left1"],
    "react": ["react@18.0.0", "https://r/react.tgz", { "dependencies": { "loose": "1" } }, "sha512-r18"],
    "web/react": ["react@19.0.0", "https://r/react.tgz", { "dependencies": { "loose": "1" } }, "sha512-r19"],
    "loose": ["loose@1.0.0", "https://r/loose.tgz", { "dependencies": { "@s/inner": "1" } }, "sha512-l1"],
    "web/react/loose": ["loose@1.1.0", "https://r/loose.tgz", {}, "sha512-l11"],
    "@s/inner": ["@s/inner@1.0.0", "https://r/inner.tgz", {}, "sha512-in"],
  }
}"#;

    fn lock() -> Parsed {
        parse(LOCK.as_bytes())
    }

    fn no_patch(_: &str) -> Option<[u8; 32]> {
        None
    }

    fn idents(p: &Parsed, ws: &str) -> Vec<String> {
        // Re-derives the closure's members through single-package probes.
        let l = Lock::view(p).unwrap();
        let entry = l.workspace(ws).unwrap();
        let base = l.closure(entry, ws, &no_patch);
        let mut found = Vec::new();
        for (_, e) in l.packages {
            let Value::Array(a) = e else { continue };
            let Value::Str(id) = &a[0] else { continue };
            // A patch on exactly this ident changes the fingerprint iff it is in the closure.
            let probe = l.closure(entry, ws, &|i| (i == id).then_some([1; 32]));
            if probe != base {
                found.push(id.clone());
            }
        }
        found.sort();
        found
    }

    #[test]
    fn segments_keep_scoped_names_whole() {
        assert_eq!(segments("a/b/@s/n"), ["a", "b", "@s/n"]);
        assert_eq!(segments("@s/n"), ["@s/n"]);
        assert_eq!(segments("web"), ["web"]);
        assert_eq!(segments(""), Vec::<&str>::new());
    }

    #[test]
    fn candidates_walk_parents_then_bare() {
        assert_eq!(
            candidates("a/b/@s/n", "d"),
            ["a/b/@s/n/d", "a/b/d", "a/d", "d"]
        );
        assert_eq!(candidates("", "d"), ["d"]);
    }

    #[test]
    fn nested_keys_resolve_by_requester() {
        let p = lock();
        // The root takes hoisted react 18; web takes its nested react 19 and
        // the loose it requests resolves under react's own nested key.
        assert_eq!(
            idents(&p, ""),
            ["@s/inner@1.0.0", "loose@1.0.0", "react@18.0.0"]
        );
        assert_eq!(
            idents(&p, "apps/web"),
            ["left@1.0.0", "loose@1.1.0", "react@19.0.0"]
        );
    }

    #[test]
    fn closure_stops_at_workspace_entries() {
        let p = lock();
        // web depends on @asc/ui, a workspace entry: not part of web's closure.
        assert!(
            !idents(&p, "apps/web")
                .iter()
                .any(|i| i.contains("workspace"))
        );
        assert_eq!(idents(&p, "packages/ui"), ["left@1.0.0"]);
    }

    #[test]
    fn patches_enter_the_fingerprint() {
        let p = lock();
        let l = Lock::view(&p).unwrap();
        let web = l.workspace("apps/web").unwrap();
        let plain = l.closure(web, "web", &no_patch);
        let patched = l.closure(web, "web", &|i| (i == "left@1.0.0").then_some([3; 32]));
        assert_ne!(plain, patched);
        let root = l.workspace("").unwrap();
        // The root does not reach left, so the patch changes nothing there.
        assert_eq!(
            l.closure(root, "root", &no_patch),
            l.closure(root, "root", &|i| (i == "left@1.0.0").then_some([3; 32]))
        );
    }

    #[test]
    fn unusable_inputs_are_values_not_errors() {
        for bytes in [&b"not json"[..], b"[]", b"{\"packages\": {\"a\": 3}}"] {
            let p = parse(bytes);
            assert!(
                Lock::view(&p).is_err(),
                "{:?}",
                String::from_utf8_lossy(bytes)
            );
        }
        assert_eq!(
            Lock::view(&Parsed(Value::Null)).unwrap_err(),
            "not a bun.lock"
        );
    }

    #[test]
    fn over_limit_lock_is_unusable() {
        let p = parse_with(LOCK.as_bytes(), LOCK.len() - 1, 100);
        assert!(Lock::view(&p).unwrap_err().contains("larger than"));
        let p = parse_with(LOCK.as_bytes(), LOCK.len(), 3);
        assert!(Lock::view(&p).unwrap_err().contains("more than 3 packages"));
        assert!(Lock::view(&parse_with(LOCK.as_bytes(), LOCK.len(), 7)).is_ok());
    }

    #[test]
    fn looks_like_lock_tells_locks_from_patches() {
        assert!(looks_like_lock(b"\n  {\"a\":1}"));
        assert!(looks_like_lock(b"// c\n{}"));
        assert!(!looks_like_lock(b"diff --git a/x b/x"));
        assert!(!looks_like_lock(b""));
    }

    #[test]
    fn parse_is_reduced_and_canonical() {
        let p = lock();
        p.0.validate().unwrap();
        assert_eq!(p, parse(LOCK.as_bytes()));
        // Registry URLs are not kept.
        assert!(!format!("{p:?}").contains("https://"));
    }
}
