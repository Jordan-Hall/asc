//! The git-fidelity lint (PLAN Q01, `docs/git-fidelity.md`).
//!
//! It reads the four registers (`docs/git-quirks.md`, `git-defects.md`, `divergences.md`,
//! `adapter-gaps.md`; one `###` section per entry, the fields of the "Register grammar") and the
//! places that cite them: `tests/compat/allowed.toml`, `tests/traces/pending.toml`, the
//! `# fidelity:` marker of a `.t` scenario, the `fidelity:` comment above a Rust test and the
//! `git-fidelity <id>` text of a refusal in non-test source. It returns every problem at once.
//!
//! The crate that includes this file by `#[path]` owns the tests; nothing here touches git.
#![allow(dead_code)] // The lint is included by a test crate; not every item is used there.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Deserialize;

/// Which register an entry lives in; the id letter follows from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Register {
    Quirk,
    Defect,
    Divergence,
    Gap,
}

impl Register {
    pub const ALL: [Register; 4] = [
        Register::Quirk,
        Register::Defect,
        Register::Divergence,
        Register::Gap,
    ];

    fn letter(self) -> char {
        match self {
            Register::Quirk => 'Q',
            Register::Defect => 'U',
            Register::Divergence => 'D',
            Register::Gap => 'A',
        }
    }

    /// The register file, relative to the repository root.
    pub fn file(self) -> &'static str {
        match self {
            Register::Quirk => "docs/git-quirks.md",
            Register::Defect => "docs/git-defects.md",
            Register::Divergence => "docs/divergences.md",
            Register::Gap => "docs/adapter-gaps.md",
        }
    }
}

const ALLOWED: &str = "tests/compat/allowed.toml";
const PENDING: &str = "tests/traces/pending.toml";
const SCENARIOS: &str = "tests/compat/scenarios";

const DECISIONS: [&str; 3] = ["replicate", "refuse", "idealize"];
const REFUSALS: [&str; 2] = ["error", "omission"];
const CLASSES: [&str; 3] = ["workspace-gitdir", "auth", "branchless"];
const KINDS: [&str; 3] = ["spurious", "blind", "drift"];

/// The text of every file the lint reads that a self-check may alter.
#[derive(Clone, Debug, Default)]
pub struct Texts {
    pub quirks: String,
    pub defects: String,
    pub divergences: String,
    pub gaps: String,
    pub allowed: String,
    pub pending: String,
}

impl Texts {
    /// The real files under `root`.
    pub fn read(root: &Path) -> Result<Texts, String> {
        let get = |rel: &str| {
            fs::read(root.join(rel))
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .map_err(|e| format!("{rel}: {e}"))
        };
        Ok(Texts {
            quirks: get(Register::Quirk.file())?,
            defects: get(Register::Defect.file())?,
            divergences: get(Register::Divergence.file())?,
            gaps: get(Register::Gap.file())?,
            allowed: get(ALLOWED)?,
            pending: get(PENDING)?,
        })
    }

    fn of(&self, r: Register) -> &str {
        match r {
            Register::Quirk => &self.quirks,
            Register::Defect => &self.defects,
            Register::Divergence => &self.divergences,
            Register::Gap => &self.gaps,
        }
    }
}

/// One `###` section.
#[derive(Clone, Debug)]
pub struct Entry {
    pub id: String,
    pub register: Register,
    /// 1-based line of the heading.
    pub line: usize,
    /// `(name, value)` of each `- **Name:** value` line, in order.
    pub fields: Vec<(String, String)>,
}

impl Entry {
    fn all(&self, name: &str) -> Vec<&str> {
        self.fields
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// The value of a field that must occur exactly once.
    fn one(&self, name: &str) -> Option<&str> {
        match self.all(name).as_slice() {
            [v] => Some(v),
            _ => None,
        }
    }

    /// The first word of a field's value (`replicate (the output ...)` is `replicate`).
    fn word(&self, name: &str) -> Option<String> {
        self.one(name)
            .and_then(|v| v.split_whitespace().next())
            .map(|w| {
                w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-')
                    .to_owned()
            })
    }

    pub fn decision(&self) -> Option<String> {
        self.word("Decision")
    }

    pub fn status(&self) -> Option<String> {
        self.word("Status")
    }

    fn label(&self) -> String {
        format!("{} ({}:{})", self.id, self.register.file(), self.line)
    }
}

/// One bullet under `## Upstream` of `divergences.md`.
#[derive(Clone, Debug)]
pub struct Bullet {
    pub id: String,
    /// The word in parentheses after the id, when there is one.
    pub decision: Option<String>,
    pub line: usize,
}

/// One register, parsed.
#[derive(Debug, Default)]
pub struct Parsed {
    pub entries: Vec<Entry>,
    pub upstream: Vec<Bullet>,
    pub errors: Vec<String>,
}

/// Parses one register file.
pub fn parse_register(register: Register, text: &str) -> Parsed {
    let heading = Regex::new(r"^### ([A-Z]-[0-9]+) \S").expect("heading regex");
    let field = Regex::new(r"^- \*\*([A-Za-z][A-Za-z ]*):\*\*\s*(.*)$").expect("field regex");
    let bullet =
        Regex::new(r"^- \*\*([A-Z]-[0-9]+)\*\*(?:\s*\(([A-Za-z]+)\))?").expect("bullet regex");
    let mut out = Parsed::default();
    let mut section = String::new();
    let mut fence = false;
    for (i, line) in text.lines().enumerate() {
        let n = i + 1;
        if line.starts_with("```") {
            fence = !fence;
            continue;
        }
        if fence {
            continue;
        }
        if let Some(h) = line.strip_prefix("## ") {
            section = h.trim().to_owned();
            continue;
        }
        if line.starts_with("### ") {
            match heading.captures(line) {
                Some(c) => {
                    let id = c[1].to_owned();
                    if !id.starts_with(register.letter()) {
                        out.errors.push(format!(
                            "{}:{n}: id {id} does not belong in this register (expected {}-n)",
                            register.file(),
                            register.letter()
                        ));
                    }
                    out.entries.push(Entry {
                        id,
                        register,
                        line: n,
                        fields: Vec::new(),
                    });
                }
                None => out.errors.push(format!(
                    "{}:{n}: heading is not `### <id> <title>` with an id such as {}-1",
                    register.file(),
                    register.letter()
                )),
            }
            continue;
        }
        if register == Register::Divergence && section == "Upstream" {
            if let Some(c) = bullet.captures(line) {
                out.upstream.push(Bullet {
                    id: c[1].to_owned(),
                    decision: c.get(2).map(|m| m.as_str().to_owned()),
                    line: n,
                });
            }
            continue;
        }
        if let (Some(e), Some(c)) = (out.entries.last_mut(), field.captures(line)) {
            // Only fields that follow a heading of this section belong to the entry.
            if section_owns(&section) {
                e.fields
                    .push((c[1].trim().to_owned(), c[2].trim().to_owned()));
            }
        }
    }
    out
}

/// Entries live under `##` sections other than `Upstream`.
fn section_owns(section: &str) -> bool {
    section != "Upstream"
}

#[derive(Deserialize)]
struct AllowFile {
    #[serde(default)]
    allow: Vec<AllowRow>,
}

#[derive(Deserialize)]
struct AllowRow {
    #[serde(default)]
    scenario: String,
    #[serde(default)]
    register: Option<String>,
}

#[derive(Deserialize)]
struct PendingFile {
    #[serde(default)]
    pending: Vec<PendingRow>,
}

#[derive(Deserialize)]
struct PendingRow {
    #[serde(default)]
    trace: String,
    #[serde(default)]
    differs: bool,
    #[serde(default)]
    register: Option<String>,
}

/// What the lint found.
#[derive(Debug, Default)]
pub struct Report {
    /// Problems; the run fails when this is not empty.
    pub errors: Vec<String>,
    /// Entries that pass a default run but not `ASC_FIDELITY_STRICT=1`: `pending <unit>` and `open`.
    pub listed: Vec<String>,
    /// Entries seen, by id.
    pub entries: BTreeMap<String, Entry>,
    /// The ids the four `[[allow]]` style files cite: `allowed.toml` in file order.
    pub allow_registers: Vec<Option<String>>,
}

impl Report {
    /// The errors, one per line, for a failing assertion.
    pub fn problems(&self) -> String {
        self.errors.join("\n")
    }

    fn err(&mut self, msg: impl Into<String>) {
        self.errors.push(msg.into());
    }
}

/// Files that cite ids, found by walking the tree once.
#[derive(Default)]
struct Scan {
    /// id to the files that carry a marker for it.
    cited: BTreeMap<String, BTreeSet<String>>,
    /// `git-fidelity <id>` in non-test source under `crates/`: id to files.
    refusals: BTreeMap<String, BTreeSet<String>>,
    problems: Vec<String>,
}

fn skip_dir(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "target" | "node_modules")
}

fn walk(dir: &Path, rel: &str, files: &mut Vec<(PathBuf, String)>) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    let mut items: Vec<_> = rd.flatten().collect();
    items.sort_by_key(|e| e.file_name());
    for e in items {
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(ft) = e.file_type() else {
            continue;
        };
        let child = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        if ft.is_dir() {
            if !skip_dir(&name) {
                walk(&e.path(), &child, files);
            }
        } else if ft.is_file() {
            files.push((e.path(), child));
        }
    }
}

fn ids_in(list: &str) -> Vec<String> {
    list.split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

/// True for source that ships: under `crates/<c>/src`, not a test module file.
fn is_product_source(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    let file = parts.last().copied().unwrap_or_default();
    parts.first() == Some(&"crates")
        && parts.get(2) == Some(&"src")
        && !parts.contains(&"tests")
        && file.ends_with(".rs")
        && file != "tests.rs"
        && file != "test.rs"
        && !file.ends_with("_tests.rs")
}

fn scan(root: &Path) -> Scan {
    let comment = Regex::new(r"^\s*//\s*fidelity:\s*([A-Z]-[0-9]+(?:\s*,\s*[A-Z]-[0-9]+)*)")
        .expect("comment regex");
    let refusal = Regex::new(r"git-fidelity ([A-Z]-[0-9]+)").expect("refusal regex");
    let marker = Regex::new(r"^#\s*fidelity:\s*(.*)$").expect("marker regex");
    let mut s = Scan::default();
    let mut files = Vec::new();
    for top in ["crates", "tests"] {
        walk(&root.join(top), top, &mut files);
    }
    for (path, rel) in files {
        let rs = rel.ends_with(".rs");
        let scenario = rel.starts_with(&format!("{SCENARIOS}/")) && rel.ends_with(".t");
        if !rs && !scenario {
            continue;
        }
        let text = match fs::read(&path) {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(e) => {
                s.problems.push(format!("{rel}: {e}"));
                continue;
            }
        };
        if scenario {
            for line in text.lines().take(5) {
                if let Some(c) = marker.captures(line.trim_end()) {
                    for id in ids_in(&c[1]) {
                        s.cited.entry(id).or_default().insert(rel.clone());
                    }
                }
            }
        }
        if rs {
            for line in text.lines() {
                if let Some(c) = comment.captures(line) {
                    for id in ids_in(&c[1]) {
                        s.cited.entry(id).or_default().insert(rel.clone());
                    }
                }
            }
            if is_product_source(&rel) {
                // Text after the first `#[cfg(test)]` at the left margin is test code.
                let product: String = text
                    .lines()
                    .take_while(|l| l.trim_end() != "#[cfg(test)]")
                    .collect::<Vec<_>>()
                    .join("\n");
                for c in refusal.captures_iter(&product) {
                    s.refusals
                        .entry(c[1].to_owned())
                        .or_default()
                        .insert(rel.clone());
                }
            }
        }
    }
    s
}

/// A path named in a `Proof` field: has a slash and a file extension we know.
fn is_path(t: &str) -> bool {
    t.contains('/')
        && [".rs", ".t", ".toml", ".md", ".sh", ".yml", ".yaml"]
            .iter()
            .any(|e| t.ends_with(e))
}

fn is_ident(t: &str) -> bool {
    !t.is_empty()
        && !t.starts_with(|c: char| c.is_ascii_digit())
        && t.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn has_fn(text: &str, name: &str) -> bool {
    Regex::new(&format!(r"\bfn\s+{}\b", regex::escape(name))).is_ok_and(|re| re.is_match(text))
}

/// Checks the `Proof` of one `proven` entry: at least one file, each file there, each named test
/// defined in the file named before it.
fn check_proof(root: &Path, e: &Entry, value: &str, r: &mut Report) {
    let token = Regex::new(r"`([^`]+)`").expect("token regex");
    let mut last_rs: Option<(String, String)> = None;
    let mut paths = 0;
    for c in token.captures_iter(value) {
        let t = &c[1];
        let (p, name) = match t.split_once("::") {
            Some((p, n)) => (p, Some(n)),
            None => (t, None),
        };
        if is_path(p) {
            paths += 1;
            let text = match fs::read(root.join(p)) {
                Ok(b) => String::from_utf8_lossy(&b).into_owned(),
                Err(_) => {
                    r.err(format!(
                        "{}: Proof names `{p}`, which does not exist",
                        e.label()
                    ));
                    last_rs = None;
                    continue;
                }
            };
            if let Some(n) = name
                && !has_fn(&text, n)
            {
                r.err(format!(
                    "{}: Proof names `{p}::{n}`, but {p} has no fn {n}",
                    e.label()
                ));
            }
            last_rs = p.ends_with(".rs").then(|| (p.to_owned(), text));
        } else if name.is_none()
            && is_ident(t)
            && let Some((p, text)) = &last_rs
            && !has_fn(text, t)
        {
            r.err(format!(
                "{}: Proof names test `{t}`, but {p} has no fn {t}",
                e.label()
            ));
        }
    }
    if paths == 0 {
        r.err(format!(
            "{}: a proven entry needs a file in its Proof",
            e.label()
        ));
    }
}

/// Runs every check. `strict` is `ASC_FIDELITY_STRICT=1`: `pending` and `open` entries fail too.
pub fn lint(root: &Path, texts: &Texts, strict: bool) -> Report {
    let mut r = Report::default();
    let mut entries: Vec<Entry> = Vec::new();
    let mut bullets: Vec<Bullet> = Vec::new();
    for reg in Register::ALL {
        let p = parse_register(reg, texts.of(reg));
        r.errors.extend(p.errors);
        entries.extend(p.entries);
        bullets.extend(p.upstream);
    }

    // Ids: well formed (the heading regex), unique across all four registers.
    for e in &entries {
        if let Some(first) = r.entries.get(&e.id) {
            let msg = format!("{}: duplicate id, first at {}", e.label(), first.label());
            r.err(msg);
        } else {
            r.entries.insert(e.id.clone(), e.clone());
        }
    }

    // What cites what.
    let found = scan(root);
    r.errors.extend(found.problems.iter().cloned());
    let mut cited: BTreeMap<String, BTreeSet<String>> = found.cited.clone();
    match toml::from_str::<AllowFile>(&texts.allowed) {
        Err(e) => r.err(format!("{ALLOWED}: {e}")),
        Ok(f) => {
            for (i, a) in f.allow.iter().enumerate() {
                let what = format!("{ALLOWED} entry {} (scenario `{}`)", i + 1, a.scenario);
                r.allow_registers.push(a.register.clone());
                match a.register.as_deref() {
                    None => r.err(format!("{what}: no `register` key")),
                    Some(id) => match r.entries.get(id) {
                        None => r.err(format!("{what}: register `{id}` does not exist")),
                        Some(e) if e.register == Register::Gap => r.err(format!(
                            "{what}: register `{id}` is an adapter gap; an allowed difference is a quirk, defect or divergence"
                        )),
                        Some(_) => {
                            cited.entry(id.to_owned()).or_default().insert(ALLOWED.to_owned());
                        }
                    },
                }
            }
        }
    }
    match toml::from_str::<PendingFile>(&texts.pending) {
        Err(e) => r.err(format!("{PENDING}: {e}")),
        Ok(f) => {
            for p in &f.pending {
                let what = format!("{PENDING} trace `{}`", p.trace);
                match p.register.as_deref() {
                    None if p.differs => {
                        r.err(format!("{what}: `differs = true` needs a `register` key"))
                    }
                    None => {}
                    Some(id) => {
                        if r.entries.contains_key(id) {
                            cited
                                .entry(id.to_owned())
                                .or_default()
                                .insert(PENDING.to_owned());
                        } else {
                            r.err(format!("{what}: register `{id}` does not exist"));
                        }
                    }
                }
            }
        }
    }
    for (id, files) in found.cited.iter().chain(found.refusals.iter()) {
        if !r.entries.contains_key(id) {
            let first = files.iter().next().cloned().unwrap_or_default();
            r.err(format!("{first}: cites `{id}`, which is in no register"));
        }
    }

    // Per entry.
    let mut upstream_ids: BTreeMap<String, &Bullet> = BTreeMap::new();
    for b in &bullets {
        if upstream_ids.insert(b.id.clone(), b).is_some() {
            r.err(format!(
                "{}:{}: two upstream bullets for {}",
                Register::Divergence.file(),
                b.line,
                b.id
            ));
        }
    }
    let unit_re = Regex::new(r"^[A-Z][0-9]{2}[a-z]?$").expect("unit regex");
    for e in &entries {
        let label = e.label();
        let status = e.status();
        let retired = matches!(status.as_deref(), Some("retired" | "superseded"));
        match e.register {
            Register::Quirk | Register::Defect => {
                let decision = e.decision();
                match decision.as_deref() {
                    Some(d) if DECISIONS.contains(&d) => {}
                    _ => r.err(format!(
                        "{label}: needs exactly one Decision of replicate, refuse or idealize"
                    )),
                }
                match (decision.as_deref(), e.all("Refusal").as_slice()) {
                    (Some("refuse"), _) => match e.word("Refusal").as_deref() {
                        Some(k) if REFUSALS.contains(&k) => {}
                        _ => r.err(format!(
                            "{label}: a refuse needs one Refusal of error or omission"
                        )),
                    },
                    (_, []) => {}
                    _ => r.err(format!("{label}: only a refuse has a Refusal")),
                }
                if !retired && matches!(decision.as_deref(), Some("refuse" | "idealize")) {
                    let said = decision.as_deref().unwrap_or_default();
                    match upstream_ids.get(&e.id) {
                        None => r.err(format!(
                            "{label}: a {said} has no bullet under `## Upstream` of {}",
                            Register::Divergence.file()
                        )),
                        Some(b) => {
                            if let Some(d) = b.decision.as_deref().filter(|d| *d != said) {
                                r.err(format!(
                                    "{}:{}: bullet says ({d}) but {} is {said}",
                                    Register::Divergence.file(),
                                    b.line,
                                    e.id
                                ));
                            }
                        }
                    }
                }
                if decision.as_deref() == Some("refuse")
                    && e.word("Refusal").as_deref() == Some("error")
                    && !retired
                    && !found.refusals.contains_key(&e.id)
                {
                    r.err(format!(
                        "{label}: a refuse with Refusal error needs the text `git-fidelity {}` in a non-test source file under crates/",
                        e.id
                    ));
                }
                if e.one("Git").is_none() || e.one("Ideal").is_none() {
                    r.err(format!("{label}: needs one Git and one Ideal field"));
                }
            }
            Register::Divergence => match e.word("Class").as_deref() {
                Some(c) if CLASSES.contains(&c) => {}
                _ => r.err(format!(
                    "{label}: needs exactly one Class of workspace-gitdir, auth or branchless"
                )),
            },
            Register::Gap => match e.word("Kind").as_deref() {
                Some(k) if KINDS.contains(&k) => {}
                _ => r.err(format!(
                    "{label}: needs exactly one Kind of spurious, blind or drift"
                )),
            },
        }

        // Status, SPEC.
        if e.one("SPEC").is_none_or(|s| !s.contains('§')) {
            r.err(format!(
                "{label}: needs one SPEC field naming a section (§n.n)"
            ));
        }
        let valid: &[&str] = if e.register == Register::Gap {
            &["open", "closed", "retired"]
        } else {
            &["proven", "pending", "open", "retired", "superseded"]
        };
        match status.as_deref() {
            Some(s) if valid.contains(&s) => {}
            _ => r.err(format!(
                "{label}: Status must be one of {}",
                valid.join(", ")
            )),
        }
        if status.as_deref() == Some("pending") {
            let unit = e
                .one("Status")
                .and_then(|s| s.split_whitespace().nth(1))
                .unwrap_or_default();
            if !unit_re.is_match(unit) {
                r.err(format!(
                    "{label}: `pending` needs the PLAN unit that writes the proof"
                ));
            }
        }
        if matches!(status.as_deref(), Some("pending" | "open")) {
            r.listed
                .push(format!("{}: {}", e.id, e.one("Status").unwrap_or_default()));
            if strict {
                r.err(format!(
                    "{label}: Status `{}` fails under ASC_FIDELITY_STRICT=1",
                    e.one("Status").unwrap_or_default()
                ));
            }
        }
        if e.register != Register::Gap {
            match e.one("Proof") {
                None => r.err(format!("{label}: needs exactly one Proof")),
                Some(p) if status.as_deref() == Some("proven") => {
                    check_proof(root, e, p, &mut r);
                    if !cited.contains_key(&e.id) {
                        r.err(format!(
                            "{label}: proven, but no file cites it (a `# fidelity:` marker in a scenario, a fidelity comment above a test, or a `register` key)"
                        ));
                    }
                }
                Some(_) => {}
            }
        }
    }

    // Upstream bullets.
    for b in &bullets {
        let at = format!("{}:{}", Register::Divergence.file(), b.line);
        match r.entries.get(&b.id) {
            None => r.err(format!(
                "{at}: upstream bullet cites {}, which is in no register",
                b.id
            )),
            Some(e) if !matches!(e.register, Register::Quirk | Register::Defect) => {
                r.err(format!(
                    "{at}: upstream bullet cites {}, which is not a quirk or defect",
                    b.id
                ));
            }
            Some(e) if e.decision().as_deref() == Some("replicate") => {
                r.err(format!(
                    "{at}: upstream bullet cites {}, whose decision is replicate",
                    b.id
                ));
            }
            Some(_) => {}
        }
    }
    r
}

/// True when `ASC_FIDELITY_STRICT=1`.
pub fn strict_from_env() -> bool {
    std::env::var("ASC_FIDELITY_STRICT").is_ok_and(|v| v == "1")
}
