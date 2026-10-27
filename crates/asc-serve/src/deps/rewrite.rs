//! Moving a pin: the text edits behind `ProposeUpdate`.
//!
//! Every function here is a pure edit of one file's text. It changes the version of one package and
//! nothing else (no reformatting, no reordering), and answers:
//!
//! * `Ok(Some(done))`: the new text;
//! * `Ok(None)`: the file does not need to change (it does not name the package at that version, or its
//!   requirement already admits the new one);
//! * `Err(reason)`: the file names the package but cannot be edited safely, and the reason says why.
//!
//! No resolver runs, so only pins that stand alone are rewritten. A lockfile entry that carries the hash
//! of what it pins (`Cargo.lock`, `package-lock.json`) is rewritten only when the hash of the new version
//! is known, from the registry; lockfiles that repeat a package per requirement or hold hashes of a whole
//! graph (`yarn.lock`, `pnpm-lock.yaml`, `poetry.lock`, `uv.lock`, `Pipfile.lock`, `composer.lock`,
//! `packages.lock.json`, `mix.lock`, `pubspec.lock`, `Package.resolved`, `Podfile.lock`, `go.sum`) are
//! not. Dependencies of the new version that the old one did not have are the package manager's to add on
//! the next install; the proposal says so.

use regex::Regex;

use super::model::Ecosystem;
use super::parse::Format;
use super::requirement::{Bump, Style, bump};
use super::version::is_safe_literal;

/// One package and the version it moves between.
#[derive(Clone, Copy, Debug)]
pub struct Pin<'a> {
    pub eco: Ecosystem,
    pub name: &'a str,
    pub from: &'a str,
    pub to: &'a str,
}

/// What the registry says about the new version, and what an earlier file of the same update decided.
#[derive(Clone, Debug, Default)]
pub struct Extra {
    /// The SHA-256 of the crate (`Cargo.lock`).
    pub checksum: Option<String>,
    /// The `sha512-...` integrity and the tarball URL of the package (`package-lock.json`).
    pub integrity: Option<String>,
    pub tarball: Option<String>,
    /// The requirement the manifest now carries, for the lockfile that repeats it.
    pub requirement: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Done {
    pub text: String,
    /// The requirement written, when the edit was of a requirement.
    pub requirement: Option<String>,
}

pub type Outcome = Result<Option<Done>, String>;

/// Why a format is never rewritten, or `None` when it can be.
pub fn unsupported(format: Format) -> Option<&'static str> {
    Some(match format {
        Format::YarnLock => "yarn.lock holds one entry per requested range and needs `yarn`",
        Format::PnpmLock => "pnpm-lock.yaml needs `pnpm` to rewrite its snapshots",
        Format::PoetryLock | Format::UvLock => "this lockfile holds hashes of the whole graph",
        Format::PipfileLock => "Pipfile.lock holds hashes; run `pipenv lock`",
        Format::Pipfile => "Pipfile requirements are not edited",
        Format::NugetLock => "packages.lock.json holds content hashes; run `dotnet restore`",
        Format::ComposerLock => "composer.lock holds hashes; run `composer update`",
        Format::MixLock => "mix.lock holds hashes; run `mix deps.update`",
        Format::PubspecLock => "pubspec.lock holds hashes; run `dart pub upgrade`",
        Format::PackageResolved | Format::PackageSwift => "SwiftPM pins hold revisions",
        Format::PodfileLock => "Podfile.lock holds checksums; run `pod update`",
        Format::GoSum => "go.sum holds module hashes; run `go mod tidy`",
        _ => return None,
    })
}

// ------------------------------------------------------------------------------ edits

struct Edits(Vec<(usize, usize, String)>);

impl Edits {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn push(&mut self, start: usize, end: usize, new: impl Into<String>) {
        self.0.push((start, end, new.into()));
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn apply(mut self, text: &str) -> String {
        self.0.sort_by_key(|e| std::cmp::Reverse(e.0));
        let mut out = text.to_owned();
        let mut floor = usize::MAX;
        for (s, e, new) in self.0 {
            // Edits never overlap; one that does is dropped rather than corrupting the text.
            if e <= floor {
                out.replace_range(s..e, &new);
                floor = s;
            }
        }
        out
    }
}

/// A regex that matches the package name in the spelling the ecosystem allows.
fn name_re(eco: Ecosystem, name: &str) -> String {
    match eco {
        Ecosystem::PyPi => {
            let parts: Vec<String> = eco.canonical(name).split('-').map(regex::escape).collect();
            format!("(?i:{})", parts.join("[-_.]+"))
        }
        Ecosystem::CratesIo => {
            let parts: Vec<String> = name.split(['-', '_']).map(regex::escape).collect();
            parts.join("[-_]")
        }
        _ => regex::escape(name),
    }
}

fn re(pattern: &str) -> Result<Regex, String> {
    Regex::new(pattern).map_err(|e| format!("internal pattern error: {e}"))
}

fn plain_error(name: &str, req: &str) -> String {
    format!("the requirement {req:?} of {name} is not a plain version and is not rewritten")
}

// ------------------------------------------------------------------------------ JSON

struct Member {
    key: String,
    val_start: usize,
    val_end: usize,
    kind: u8,
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// The index after the string that starts at `i`.
fn skip_string(b: &[u8], mut i: usize) -> Option<usize> {
    i += 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// The index after the value that starts at `i`.
fn skip_value(b: &[u8], mut i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'"' => skip_string(b, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            while i < b.len() {
                match b[i] {
                    b'"' => {
                        i = skip_string(b, i)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            None
        }
        _ => {
            while i < b.len() && !matches!(b[i], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
            {
                i += 1;
            }
            Some(i)
        }
    }
}

/// The members of the object whose `{` is at `open`.
fn members(text: &str, open: usize) -> Option<Vec<Member>> {
    let b = text.as_bytes();
    if b.get(open) != Some(&b'{') {
        return None;
    }
    let mut out = Vec::new();
    let mut i = skip_ws(b, open + 1);
    if b.get(i) == Some(&b'}') {
        return Some(out);
    }
    loop {
        if b.get(i) != Some(&b'"') {
            return None;
        }
        let key_end = skip_string(b, i)?;
        let key = text[i + 1..key_end - 1].to_owned();
        i = skip_ws(b, key_end);
        if b.get(i) != Some(&b':') {
            return None;
        }
        i = skip_ws(b, i + 1);
        let end = skip_value(b, i)?;
        out.push(Member {
            key,
            val_start: i,
            val_end: end,
            kind: b[i],
        });
        i = skip_ws(b, end);
        match b.get(i)? {
            b',' => i = skip_ws(b, i + 1),
            b'}' => return Some(out),
            _ => return None,
        }
    }
}

impl Member {
    fn is_str(&self) -> bool {
        self.kind == b'"'
    }

    fn is_obj(&self) -> bool {
        self.kind == b'{'
    }

    fn string<'a>(&self, text: &'a str) -> &'a str {
        if self.is_str() {
            &text[self.val_start + 1..self.val_end - 1]
        } else {
            ""
        }
    }

    /// Edits the content of a string value.
    fn set(&self, edits: &mut Edits, new: &str) {
        edits.push(self.val_start + 1, self.val_end - 1, json_escape(new));
    }
}

/// `s` as the inside of a JSON string.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

fn find<'a>(ms: &'a [Member], key: &str) -> Option<&'a Member> {
    ms.iter().find(|m| m.key == key)
}

fn root_members(text: &str) -> Result<Vec<Member>, String> {
    let open = skip_ws(text.as_bytes(), 0);
    members(text, open).ok_or_else(|| "the file is not a JSON object".to_owned())
}

/// Rewrites `"name": "requirement"` in the dependency sections `sections` of a JSON manifest.
fn json_requirements(text: &str, sections: &[&str], pin: &Pin<'_>, style: Style) -> Outcome {
    let top = root_members(text)?;
    let mut edits = Edits::new();
    let mut written = None;
    for section in sections {
        let Some(sec) = find(&top, section).filter(|m| m.is_obj()) else {
            continue;
        };
        let Some(deps) = members(text, sec.val_start) else {
            continue;
        };
        let Some(m) = find(&deps, pin.name).filter(|m| m.is_str()) else {
            continue;
        };
        let req = m.string(text);
        match bump(style, req, pin.to) {
            Bump::Unchanged => {}
            Bump::Changed(new) => {
                m.set(&mut edits, &new);
                written.get_or_insert(new);
            }
            Bump::Complex => return Err(plain_error(pin.name, req)),
        }
    }
    Ok((!edits.is_empty()).then(|| Done {
        text: edits.apply(text),
        requirement: written,
    }))
}

fn package_json(text: &str, pin: &Pin<'_>) -> Outcome {
    json_requirements(
        text,
        &["dependencies", "devDependencies", "optionalDependencies"],
        pin,
        Style::Exact,
    )
}

fn composer_json(text: &str, pin: &Pin<'_>) -> Outcome {
    json_requirements(text, &["require", "require-dev"], pin, Style::Exact)
}

fn lock_fields(
    text: &str,
    entry: &[Member],
    pin: &Pin<'_>,
    extra: &Extra,
    edits: &mut Edits,
) -> Result<bool, String> {
    let Some(v) = find(entry, "version").filter(|m| m.is_str()) else {
        return Ok(false);
    };
    if v.string(text) != pin.from {
        return Ok(false);
    }
    v.set(edits, pin.to);
    for (key, want) in [
        ("resolved", &extra.tarball),
        ("integrity", &extra.integrity),
    ] {
        if let Some(m) = find(entry, key).filter(|m| m.is_str()) {
            let Some(new) = want else {
                return Err(format!(
                    "the registry's {key} of {} {} is not known",
                    pin.name, pin.to
                ));
            };
            m.set(edits, new);
        }
    }
    Ok(true)
}

/// The legacy `dependencies` tree of lockfileVersion 1 and 2.
fn legacy_tree(
    text: &str,
    open: usize,
    pin: &Pin<'_>,
    extra: &Extra,
    edits: &mut Edits,
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
        if m.key == pin.name {
            lock_fields(text, &entry, pin, extra, edits)?;
        }
        if let Some(deps) = find(&entry, "dependencies").filter(|d| d.is_obj()) {
            legacy_tree(text, deps.val_start, pin, extra, edits, depth + 1)?;
        }
    }
    Ok(())
}

fn package_lock(text: &str, pin: &Pin<'_>, extra: &Extra) -> Outcome {
    let top = root_members(text)?;
    let mut edits = Edits::new();
    // The root's requirements change only when a pin did.
    let mut root = Edits::new();
    if let Some(packages) = find(&top, "packages").filter(|m| m.is_obj()) {
        let tail = format!("node_modules/{}", pin.name);
        for m in members(text, packages.val_start)
            .unwrap_or_default()
            .iter()
            .filter(|m| m.is_obj())
        {
            let Some(entry) = members(text, m.val_start) else {
                continue;
            };
            if m.key.is_empty() {
                // The root repeats the requirements of package.json.
                if let Some(req) = &extra.requirement {
                    for section in ["dependencies", "devDependencies", "optionalDependencies"] {
                        let Some(deps) = find(&entry, section).filter(|d| d.is_obj()) else {
                            continue;
                        };
                        let deps = members(text, deps.val_start).unwrap_or_default();
                        if let Some(r) = find(&deps, pin.name).filter(|r| r.is_str()) {
                            r.set(&mut root, req);
                        }
                    }
                }
                continue;
            }
            if m.key != tail && !m.key.ends_with(&format!("/{tail}")) {
                continue;
            }
            // An alias installs another package under this name.
            if find(&entry, "name")
                .filter(|n| n.is_str())
                .is_some_and(|n| n.string(text) != pin.name)
            {
                continue;
            }
            lock_fields(text, &entry, pin, extra, &mut edits)?;
        }
    }
    if let Some(tree) = find(&top, "dependencies").filter(|m| m.is_obj()) {
        legacy_tree(text, tree.val_start, pin, extra, &mut edits, 0)?;
    }
    if !edits.is_empty() {
        edits.0.append(&mut root.0);
    }
    Ok((!edits.is_empty()).then(|| Done {
        text: edits.apply(text),
        requirement: None,
    }))
}

// ------------------------------------------------------------------------------ Cargo

fn cargo_section_is_deps(section: &str) -> bool {
    let last = section.rsplit('.').next().unwrap_or(section);
    matches!(
        last,
        "dependencies" | "dev-dependencies" | "build-dependencies"
    )
}

/// `[dependencies.name]`: the section is a table of one dependency; returns the key before the name.
fn cargo_dep_table(section: &str) -> Option<&str> {
    let (head, name) = section.rsplit_once('.')?;
    cargo_section_is_deps(head).then_some(name.trim_matches('"'))
}

fn cargo_toml(text: &str, pin: &Pin<'_>) -> Outcome {
    let ver_re = re(r#"version\s*=\s*"([^"]*)""#)?;
    let pkg_re = re(r#"package\s*=\s*"([^"]*)""#)?;
    let name_is = |n: &str| pin.eco.canonical(n) == pin.eco.canonical(pin.name);
    let mut edits = Edits::new();
    let mut written = None;
    let mut section = String::new();
    // Inside `[dependencies.name]`: is the table for our package.
    let mut table_is_ours = false;
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let t = line.trim();
        if t.starts_with('[') {
            section = t
                .trim_start_matches('[')
                .split(']')
                .next()
                .unwrap_or("")
                .trim()
                .to_owned();
            table_is_ours = cargo_dep_table(&section).is_some_and(name_is);
            continue;
        }
        let mut apply = |req_start: usize, req_end: usize, req: &str| -> Result<(), String> {
            match bump(Style::Caret, req, pin.to) {
                Bump::Unchanged => Ok(()),
                Bump::Changed(new) => {
                    edits.push(start + req_start, start + req_end, new.clone());
                    written.get_or_insert(new);
                    Ok(())
                }
                Bump::Complex => Err(plain_error(pin.name, req)),
            }
        };
        if table_is_ours {
            if t.starts_with("version")
                && let Some(c) = ver_re.captures(line)
            {
                let m = c.get(1).ok_or("no version")?;
                apply(m.start(), m.end(), m.as_str())?;
            }
            continue;
        }
        if !cargo_section_is_deps(&section) {
            continue;
        }
        let Some((lhs, rhs)) = line.split_once('=') else {
            continue;
        };
        let key = lhs.trim().trim_matches('"');
        let value = rhs.trim();
        let value_at = lhs.len() + 1 + (rhs.len() - rhs.trim_start().len());
        if let Some(inner) = value.strip_prefix('"') {
            if name_is(key)
                && let Some(end) = inner.find('"')
            {
                apply(value_at + 1, value_at + 1 + end, &inner[..end])?;
            }
        } else if value.starts_with('{') {
            // The crate this entry is for: `package`, else the key.
            let real = pkg_re
                .captures(value)
                .map_or_else(|| key.to_owned(), |p| p[1].to_owned());
            if name_is(&real)
                && let Some(v) = ver_re.captures(value)
            {
                let m = v.get(1).ok_or("no version")?;
                apply(value_at + m.start(), value_at + m.end(), m.as_str())?;
            }
        }
    }
    Ok((!edits.is_empty()).then(|| Done {
        text: edits.apply(text),
        requirement: written,
    }))
}

fn cargo_lock(text: &str, pin: &Pin<'_>, extra: &Extra) -> Outcome {
    let name_pat = name_re(pin.eco, pin.name);
    let name_line = re(&format!(r#"^name\s*=\s*"({name_pat})"\s*$"#))?;
    let version_line = re(r#"^version\s*=\s*"([^"]*)"\s*$"#)?;
    let checksum_line = re(r#"^checksum\s*=\s*"([^"]*)"\s*$"#)?;
    let dep_entry = re(&format!(
        r#""({name_pat}) {}( \(|")"#,
        regex::escape(pin.from)
    ))?;
    let mut lines: Vec<(usize, &str)> = Vec::new();
    let mut offset = 0usize;
    for l in text.split_inclusive('\n') {
        lines.push((offset, l));
        offset += l.len();
    }
    let mut edits = Edits::new();
    // Blocks: [[package]] header index to the next header.
    let heads: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, (_, l))| l.trim_end() == "[[package]]")
        .map(|(i, _)| i)
        .collect();
    for (n, &h) in heads.iter().enumerate() {
        let end = heads.get(n + 1).copied().unwrap_or(lines.len());
        let block = &lines[h + 1..end];
        let find_line = |r: &Regex| {
            block
                .iter()
                .find_map(|(at, l)| r.captures(l.trim_end()).map(|c| (*at, *l, c)))
        };
        let Some(_) = find_line(&name_line) else {
            continue;
        };
        let Some((vat, vline, vc)) = find_line(&version_line) else {
            continue;
        };
        if &vc[1] != pin.from || !block.iter().any(|(_, l)| l.starts_with("source")) {
            continue;
        }
        let ver = vc.get(1).ok_or("no version")?;
        let _ = vline;
        edits.push(vat + ver.start(), vat + ver.end(), pin.to);
        if let Some((cat, _, cc)) = find_line(&checksum_line) {
            let Some(new) = &extra.checksum else {
                return Err(format!(
                    "the registry's checksum of {} {} is not known",
                    pin.name, pin.to
                ));
            };
            let old = cc.get(1).ok_or("no checksum")?;
            edits.push(cat + old.start(), cat + old.end(), new.clone());
        }
    }
    if edits.is_empty() {
        return Ok(None);
    }
    // `"name 1.2.3"` in the `dependencies` of other packages names the version when two are locked.
    for (at, l) in &lines {
        for c in dep_entry.captures_iter(l) {
            let whole = c.get(0).ok_or("no match")?;
            let from_at = whole.start() + 1 + c[1].len() + 1;
            edits.push(at + from_at, at + from_at + pin.from.len(), pin.to);
        }
    }
    Ok(Some(Done {
        text: edits.apply(text),
        requirement: None,
    }))
}

// ------------------------------------------------------------------------------ line formats

fn requirements_txt(text: &str, pin: &Pin<'_>) -> Outcome {
    let name_pat = name_re(pin.eco, pin.name);
    let r = re(&format!(
        r"^(\s*{name_pat}(?:\[[^\]]*\])?\s*===?\s*)({})(\s|;|#|\\|$)",
        regex::escape(pin.from)
    ))?;
    let lines: Vec<(usize, &str)> = {
        let mut v = Vec::new();
        let mut off = 0;
        for l in text.split_inclusive('\n') {
            v.push((off, l));
            off += l.len();
        }
        v
    };
    let mut edits = Edits::new();
    for (i, (at, line)) in lines.iter().enumerate() {
        let Some(c) = r.captures(line) else { continue };
        // Hashes pin the files of the old version.
        let mut j = i;
        let mut hashed = line.contains("--hash");
        while !hashed && lines[j].1.trim_end().ends_with('\\') && j + 1 < lines.len() {
            j += 1;
            hashed = lines[j].1.contains("--hash");
        }
        if hashed {
            return Err(format!(
                "{} is pinned with --hash options, which only pip-compile can compute",
                pin.name
            ));
        }
        let v = c.get(2).ok_or("no version")?;
        edits.push(at + v.start(), at + v.end(), pin.to);
    }
    Ok((!edits.is_empty()).then(|| Done {
        text: edits.apply(text),
        requirement: None,
    }))
}

fn pyproject(text: &str, pin: &Pin<'_>) -> Outcome {
    let name_pat = name_re(pin.eco, pin.name);
    // A PEP 508 string in an array: "name[extra] >= 1.0".
    let r = re(&format!(
        r#"(["'])(\s*{name_pat}(?:\[[^\]]*\])?\s*)(===|==|~=|>=)(\s*)([0-9][0-9A-Za-z.]*)([^"']*)(["'])"#
    ))?;
    let mut edits = Edits::new();
    let mut written = None;
    for c in r.captures_iter(text) {
        let tail = &c[6];
        let op = &c[3];
        let ver = c.get(5).ok_or("no version")?;
        if tail.trim_start().starts_with(',') {
            return Err(plain_error(
                pin.name,
                &format!("{op}{}{tail}", ver.as_str()),
            ));
        }
        let old = format!("{op}{}{}", &c[4], ver.as_str());
        match bump(Style::Pep440, &old, pin.to) {
            Bump::Unchanged => {}
            Bump::Changed(new) => {
                let op_start = c.get(3).ok_or("no operator")?.start();
                edits.push(op_start, ver.end(), new.clone());
                written.get_or_insert(new);
            }
            Bump::Complex => return Err(plain_error(pin.name, &old)),
        }
    }
    Ok((!edits.is_empty()).then(|| Done {
        text: edits.apply(text),
        requirement: written,
    }))
}

fn go_mod(text: &str, pin: &Pin<'_>) -> Outcome {
    let module = regex::escape(pin.name);
    let r = re(&format!(
        r"^(\s*(?:require\s+)?){module}(\s+)({})(\s|$)",
        regex::escape(pin.from)
    ))?;
    let mut edits = Edits::new();
    let mut block: Option<String> = None;
    let mut off = 0usize;
    for line in text.split_inclusive('\n') {
        let at = off;
        off += line.len();
        let code = line.split("//").next().unwrap_or("").trim();
        let (directive, body) = match &block {
            Some(b) => {
                if code == ")" {
                    block = None;
                    continue;
                }
                (b.clone(), code.to_owned())
            }
            None => {
                let (kw, rest) = code.split_once(char::is_whitespace).unwrap_or((code, ""));
                if rest.trim() == "(" {
                    block = Some(kw.to_owned());
                    continue;
                }
                (kw.to_owned(), rest.trim().to_owned())
            }
        };
        match directive.as_str() {
            "require" => {
                if let Some(c) = r.captures(line) {
                    let v = c.get(3).ok_or("no version")?;
                    edits.push(at + v.start(), at + v.end(), pin.to);
                }
            }
            "replace" => {
                let left = body.split("=>").next().unwrap_or("");
                if body.contains("=>") && left.split_whitespace().next() == Some(pin.name) {
                    return Err(format!(
                        "{} is replaced in go.mod; edit the replace directive by hand",
                        pin.name
                    ));
                }
            }
            _ => {}
        }
    }
    Ok((!edits.is_empty()).then(|| Done {
        text: edits.apply(text),
        requirement: None,
    }))
}

fn gradle_lock(text: &str, pin: &Pin<'_>) -> Outcome {
    let (group, artifact) = pin
        .name
        .split_once(':')
        .ok_or("a Maven name is group:artifact")?;
    let r = re(&format!(
        r"(?m)^({}:{}:)({})(=)",
        regex::escape(group),
        regex::escape(artifact),
        regex::escape(pin.from)
    ))?;
    let mut edits = Edits::new();
    for c in r.captures_iter(text) {
        let v = c.get(2).ok_or("no version")?;
        edits.push(v.start(), v.end(), pin.to);
    }
    Ok((!edits.is_empty()).then(|| Done {
        text: edits.apply(text),
        requirement: None,
    }))
}

fn pom(text: &str, pin: &Pin<'_>) -> Outcome {
    let (group, artifact) = pin
        .name
        .split_once(':')
        .ok_or("a Maven name is group:artifact")?;
    let block = re(r"(?s)<dependency>.*?</dependency>")?;
    let g_re = re(r"<groupId>\s*([^<]*?)\s*</groupId>")?;
    let a_re = re(r"<artifactId>\s*([^<]*?)\s*</artifactId>")?;
    let v_re = re(r"<version>(\s*)([^<]*?)(\s*)</version>")?;
    let mgmt = re(r"(?s)<dependencyManagement>.*?</dependencyManagement>")?;
    let mgmt_spans: Vec<(usize, usize)> =
        mgmt.find_iter(text).map(|m| (m.start(), m.end())).collect();
    let in_mgmt = |at: usize| mgmt_spans.iter().any(|(s, e)| at >= *s && at < *e);
    let mut candidates: Vec<(bool, usize, String)> = Vec::new();
    for b in block.find_iter(text) {
        let s = b.as_str();
        let (Some(g), Some(a)) = (g_re.captures(s), a_re.captures(s)) else {
            continue;
        };
        if &g[1] != group || &a[1] != artifact {
            continue;
        }
        if let Some(v) = v_re.captures(s) {
            let m = v.get(2).ok_or("no version")?;
            candidates.push((
                in_mgmt(b.start()),
                b.start() + m.start(),
                m.as_str().to_owned(),
            ));
        }
    }
    // Explicit versions of dependencies first, then the managed one.
    candidates.sort_by_key(|(managed, _, _)| *managed);
    let mut edits = Edits::new();
    for (_, at, value) in &candidates {
        if value == pin.from {
            edits.push(*at, *at + value.len(), pin.to);
        } else if let Some(prop) = value.strip_prefix("${").and_then(|v| v.strip_suffix('}')) {
            let uses = text.matches(&format!("${{{prop}}}")).count();
            let p = re(&format!(
                r"<{}>(\s*)({})(\s*)</{}>",
                regex::escape(prop),
                regex::escape(pin.from),
                regex::escape(prop)
            ))?;
            let Some(c) = p.captures(text) else { continue };
            if uses > candidates.iter().filter(|(_, _, v)| v == value).count() {
                return Err(format!(
                    "the property {prop} that holds the version is shared with other dependencies"
                ));
            }
            let m = c.get(2).ok_or("no version")?;
            edits.push(m.start(), m.end(), pin.to);
        }
        // One place holds the version; stop at the first that did.
        if !edits.is_empty() {
            break;
        }
    }
    Ok((!edits.is_empty()).then(|| Done {
        text: edits.apply(text),
        requirement: None,
    }))
}

fn csproj(text: &str, pin: &Pin<'_>) -> Outcome {
    let name = regex::escape(pin.name);
    let from = regex::escape(pin.from);
    let attr = re(&format!(
        r#"(<PackageReference\b[^>]*\bInclude\s*=\s*"{name}"[^>]*\bVersion\s*=\s*")({from})(")"#
    ))?;
    let child = re(&format!(
        r#"(?s)(<PackageReference\b[^>]*\bInclude\s*=\s*"{name}"[^>/]*>\s*<Version>\s*)({from})(\s*</Version>)"#
    ))?;
    let mut edits = Edits::new();
    for r in [&attr, &child] {
        for c in r.captures_iter(text) {
            let v = c.get(2).ok_or("no version")?;
            edits.push(v.start(), v.end(), pin.to);
        }
    }
    Ok((!edits.is_empty()).then(|| Done {
        text: edits.apply(text),
        requirement: None,
    }))
}

fn gemfile(text: &str, pin: &Pin<'_>) -> Outcome {
    let r = re(&format!(
        r#"(?m)^(\s*gem\s+['"]{}['"]\s*,\s*['"])([^'"]*)(['"])([^\n]*)"#,
        regex::escape(pin.name)
    ))?;
    let mut edits = Edits::new();
    let mut written = None;
    for c in r.captures_iter(text) {
        let req = c.get(2).ok_or("no requirement")?;
        // A second requirement string (`'>= 1', '< 2'`) is a range.
        let tail = c.get(4).map_or("", |m| m.as_str());
        let second = re(r#"^\s*,\s*['"][<>=~!0-9]"#)?;
        if second.is_match(tail) {
            return Err(plain_error(pin.name, &format!("{}, ...", req.as_str())));
        }
        match bump(Style::Gem, req.as_str(), pin.to) {
            Bump::Unchanged => {}
            Bump::Changed(new) => {
                edits.push(req.start(), req.end(), new.clone());
                written.get_or_insert(new);
            }
            Bump::Complex => return Err(plain_error(pin.name, req.as_str())),
        }
    }
    Ok((!edits.is_empty()).then(|| Done {
        text: edits.apply(text),
        requirement: written,
    }))
}

fn gemfile_lock(text: &str, pin: &Pin<'_>, extra: &Extra) -> Outcome {
    let name = regex::escape(pin.name);
    let spec = re(&format!(
        r"(?m)^(    {name} \()({})((?:-[^)]*)?\))",
        regex::escape(pin.from)
    ))?;
    let mut edits = Edits::new();
    for c in spec.captures_iter(text) {
        let v = c.get(2).ok_or("no version")?;
        edits.push(v.start(), v.end(), pin.to);
    }
    if edits.is_empty() {
        return Ok(None);
    }
    // Bundler compares the Gemfile with the DEPENDENCIES section.
    if let Some(req) = &extra.requirement {
        let dep = re(&format!(r"(?m)^(  {name} \()([^)\n]*)(\)!?)$"))?;
        for c in dep.captures_iter(text) {
            let r = c.get(2).ok_or("no requirement")?;
            edits.push(r.start(), r.end(), req.clone());
        }
    }
    Ok(Some(Done {
        text: edits.apply(text),
        requirement: None,
    }))
}

fn mix_exs(text: &str, pin: &Pin<'_>) -> Outcome {
    let r = re(&format!(
        r#"(\{{\s*:{}\s*,\s*")([^"]*)(")"#,
        regex::escape(pin.name)
    ))?;
    let mut edits = Edits::new();
    let mut written = None;
    for c in r.captures_iter(text) {
        let req = c.get(2).ok_or("no requirement")?;
        match bump(Style::Gem, req.as_str(), pin.to) {
            Bump::Unchanged => {}
            Bump::Changed(new) => {
                edits.push(req.start(), req.end(), new.clone());
                written.get_or_insert(new);
            }
            Bump::Complex => return Err(plain_error(pin.name, req.as_str())),
        }
    }
    Ok((!edits.is_empty()).then(|| Done {
        text: edits.apply(text),
        requirement: written,
    }))
}

/// Everything that is written into a file comes from a registry or a lockfile, so none of it is trusted:
/// versions are plain literals, a checksum is hex, an integrity is `shaNNN-base64`, a tarball is an https
/// URL without quotes, markup or spaces, a requirement is operators and a version.
pub fn check_inputs(pin: &Pin<'_>, extra: &Extra) -> Result<(), String> {
    if !is_safe_literal(pin.to) || !is_safe_literal(pin.from) {
        return Err(
            "a version is not a plain literal (letters, digits and . + _ -) and is not written"
                .into(),
        );
    }
    if let Some(c) = &extra.checksum
        && !(c.len() == 64
            && c.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    {
        return Err("the registry's checksum is not 64 hex digits".into());
    }
    if let Some(i) = &extra.integrity {
        let ok = ["sha256-", "sha384-", "sha512-"].iter().any(|p| {
            i.strip_prefix(p).is_some_and(|b| {
                !b.is_empty()
                    && b.len() <= 128
                    && b.bytes()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'='))
            })
        });
        if !ok {
            return Err("the registry's integrity is not shaNNN-base64".into());
        }
    }
    if let Some(t) = &extra.tarball {
        let ok = t.len() <= 1024
            && t.starts_with("https://")
            && t.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-._~:/?#@%+=,;!$&*()[]".contains(&c));
        if !ok {
            return Err("the registry's tarball is not a plain https URL".into());
        }
    }
    if let Some(r) = &extra.requirement
        && (r.len() > 64
            || !r
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b" .+_-~^<>=!".contains(&c)))
    {
        return Err("a requirement is not operators and a version".into());
    }
    Ok(())
}

/// Edits `text`, the file of `format`, to move `pin`.
pub fn rewrite(format: Format, text: &str, pin: &Pin<'_>, extra: &Extra) -> Outcome {
    if let Some(why) = unsupported(format) {
        return Err(why.to_owned());
    }
    check_inputs(pin, extra)?;
    match format {
        Format::CargoToml => cargo_toml(text, pin),
        Format::CargoLock => cargo_lock(text, pin, extra),
        Format::PackageJson => package_json(text, pin),
        Format::PackageLock => package_lock(text, pin, extra),
        Format::Requirements => requirements_txt(text, pin),
        Format::PyProject => pyproject(text, pin),
        Format::GoMod => go_mod(text, pin),
        Format::GradleLock => gradle_lock(text, pin),
        Format::PomXml => pom(text, pin),
        Format::Csproj => csproj(text, pin),
        Format::ComposerJson => composer_json(text, pin),
        Format::Gemfile => gemfile(text, pin),
        Format::GemfileLock => gemfile_lock(text, pin, extra),
        Format::MixExs => mix_exs(text, pin),
        other => Err(format!("{} is not rewritten", other.name())),
    }
}

#[cfg(test)]
#[path = "rewrite/tests.rs"]
mod tests;
