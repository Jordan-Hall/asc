//! `asc doctor --workflows` (SPEC §21.4.4, unit B24): list the workflows whose `on: push` trigger
//! can fire for a branch that is not shared.
//!
//! A branchless repo keeps non-shared branches as user refs, and a user ref emits no push event,
//! so a workflow that relies on `on: push` for such a branch never runs. The shared branches are
//! the default branch, `release/*` and `green/*` (SPEC §21.5.1). The scan reads
//! `.forgejo/workflows`, `.gitea/workflows` and `.github/workflows` under the workspace root.
//!
//! Safety: the files are untrusted. A symbolic link or reparse point is never followed (not as
//! a directory, not as a file), a file over 1 MiB is skipped with a note, at most 256 files and
//! 20000 lines per file are read, and nesting stops at 16 levels. The YAML subset parsed is the
//! one workflow triggers use (block and flow mappings, lists, scalars); anything else is
//! ignored, never executed. Nothing is written.

use std::fs;
use std::io::Read;
use std::path::Path;

use super::{Check, Level};

const DIRS: [&str; 3] = [
    ".forgejo/workflows",
    ".gitea/workflows",
    ".github/workflows",
];
const MAX_FILES: usize = 256;
const MAX_BYTES: u64 = 1 << 20;
const MAX_LINES: usize = 20_000;
const MAX_DEPTH: usize = 16;
/// The default branch, shared in a branchless repo (SPEC §21.5.1 default `shared_refs`).
const DEFAULT_BRANCH: &str = "master";

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Scalar(String),
    List(Vec<String>),
    Map(Vec<(String, Node)>),
}

impl Node {
    fn get(&self, key: &str) -> Option<&Node> {
        match self {
            Self::Map(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn strings(&self) -> Vec<String> {
        match self {
            Self::Scalar(s) if s.is_empty() => Vec::new(),
            Self::Scalar(s) => vec![s.clone()],
            Self::List(l) => l.clone(),
            Self::Map(_) => Vec::new(),
        }
    }
}

struct Line {
    indent: usize,
    text: String,
}

fn strip_comment(s: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut prev_space = true;
    for (i, c) in s.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => {
                if (c == '"' || c == '\'') && prev_space {
                    quote = Some(c);
                } else if c == '#' && prev_space {
                    return &s[..i];
                }
            }
        }
        prev_space = c == ' ' || c == '\t';
    }
    s
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

fn lines_of(src: &str) -> Vec<Line> {
    let mut out = Vec::new();
    for raw in src.lines().take(MAX_LINES) {
        let body = strip_comment(raw);
        let text = body.trim();
        if text.is_empty() || text == "---" || text == "..." {
            continue;
        }
        let indent = body.len() - body.trim_start().len();
        out.push(Line {
            indent,
            text: text.to_string(),
        });
    }
    out
}

/// Splits on commas that sit outside brackets, braces and quotes.
fn split_top(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut cur = String::new();
    for c in s.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
                cur.push(c);
            }
            None => match c {
                '"' | '\'' => {
                    quote = Some(c);
                    cur.push(c);
                }
                '[' | '{' => {
                    depth += 1;
                    cur.push(c);
                }
                ']' | '}' => {
                    depth -= 1;
                    cur.push(c);
                }
                ',' if depth == 0 => out.push(std::mem::take(&mut cur)),
                _ => cur.push(c),
            },
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// `key: value` split on the first colon that ends a key.
fn split_key(s: &str) -> Option<(String, String)> {
    let b = s.as_bytes();
    let mut quote: Option<u8> = None;
    for i in 0..b.len() {
        match quote {
            Some(q) if b[i] == q => quote = None,
            Some(_) => {}
            None => {
                if (b[i] == b'"' || b[i] == b'\'') && i == 0 {
                    quote = Some(b[i]);
                } else if b[i] == b':' && (i + 1 == b.len() || b[i + 1] == b' ') {
                    return Some((unquote(&s[..i]), s[i + 1..].trim().to_string()));
                }
            }
        }
    }
    None
}

fn inline(v: &str, depth: usize) -> Node {
    let v = v.trim();
    if let Some(body) = v.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        return Node::List(split_top(body).iter().map(|s| unquote(s)).collect());
    }
    if let Some(body) = v.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
        if depth >= MAX_DEPTH {
            return Node::Scalar(String::new());
        }
        let mut m = Vec::new();
        for part in split_top(body) {
            if let Some((k, val)) = split_key(part.trim()) {
                m.push((k, inline(&val, depth + 1)));
            }
        }
        return Node::Map(m);
    }
    let s = unquote(v);
    if s == "~" || s == "null" {
        return Node::Scalar(String::new());
    }
    Node::Scalar(s)
}

fn block(lines: &[Line], i: &mut usize, indent: usize, depth: usize) -> Node {
    if depth >= MAX_DEPTH {
        *i = lines.len();
        return Node::Scalar(String::new());
    }
    if lines[*i].text.starts_with("- ") || lines[*i].text == "-" {
        let mut items = Vec::new();
        while *i < lines.len() && lines[*i].indent == indent && lines[*i].text.starts_with('-') {
            items.push(unquote(lines[*i].text[1..].trim()));
            *i += 1;
            while *i < lines.len() && lines[*i].indent > indent {
                *i += 1;
            }
        }
        return Node::List(items);
    }
    let mut map = Vec::new();
    while *i < lines.len() && lines[*i].indent == indent {
        let text = lines[*i].text.clone();
        *i += 1;
        let Some((key, value)) = split_key(&text) else {
            continue;
        };
        if value.starts_with('|') || value.starts_with('>') {
            while *i < lines.len() && lines[*i].indent > indent {
                *i += 1;
            }
            map.push((key, Node::Scalar(String::new())));
        } else if value.is_empty() {
            let child = *i < lines.len()
                && (lines[*i].indent > indent
                    || (lines[*i].indent == indent && lines[*i].text.starts_with("- ")));
            if child {
                let ci = lines[*i].indent;
                map.push((key, block(lines, i, ci, depth + 1)));
            } else {
                map.push((key, Node::Scalar(String::new())));
            }
        } else {
            map.push((key, inline(&value, 0)));
        }
    }
    // Lines deeper than the mapping that no key claimed (continuations) are skipped.
    while *i < lines.len() && lines[*i].indent > indent {
        *i += 1;
    }
    Node::Map(map)
}

fn parse(src: &str) -> Option<Node> {
    let lines = lines_of(src);
    if lines.is_empty() {
        return None;
    }
    let mut i = 0;
    let indent = lines[0].indent;
    Some(block(&lines, &mut i, indent, 0))
}

/// What a workflow's push trigger can reach.
#[derive(Debug, PartialEq)]
enum Push {
    /// No `push` trigger, or tags only.
    None,
    /// Every pattern names a shared branch.
    Shared,
    /// Fires for these non-shared patterns; empty means every branch.
    Unshared(Vec<String>),
}

fn is_shared_pattern(p: &str) -> bool {
    p == DEFAULT_BRANCH
        || (p.starts_with("release/") && p != "release/")
        || (p.starts_with("green/") && p != "green/")
}

fn push_reach(doc: &Node) -> Push {
    // YAML 1.1 parsers read an unquoted `on` as the boolean `true`.
    let Some(on) = doc.get("on").or_else(|| doc.get("true")) else {
        return Push::None;
    };
    let push = match on {
        Node::Scalar(_) | Node::List(_) => {
            return if on.strings().iter().any(|s| s == "push") {
                Push::Unshared(Vec::new())
            } else {
                Push::None
            };
        }
        Node::Map(_) => match on.get("push") {
            Some(p) => p,
            None => return Push::None,
        },
    };
    if let Some(branches) = push.get("branches") {
        let bad: Vec<String> = branches
            .strings()
            .into_iter()
            .filter(|p| !p.starts_with('!') && !is_shared_pattern(p))
            .collect();
        return if bad.is_empty() {
            Push::Shared
        } else {
            Push::Unshared(bad)
        };
    }
    if let Some(ignore) = push.get("branches-ignore") {
        let mut v = vec!["every branch except".to_string()];
        v.extend(ignore.strings());
        return Push::Unshared(v);
    }
    if push.get("tags").is_some() || push.get("tags-ignore").is_some() {
        return Push::None;
    }
    Push::Unshared(Vec::new())
}

fn is_real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p)
        .map(|m| m.file_type().is_dir())
        .unwrap_or(false)
}

/// Every ancestor of `p` under the root was checked by `symlink_metadata` before this call.
fn list_dir(p: &Path) -> std::io::Result<fs::ReadDir> {
    fs::read_dir(p) // lint:allow-worktree-fs
}

fn read_bounded(p: &Path) -> Result<String, String> {
    let mut f = fs::File::open(p).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    f.by_ref()
        .take(MAX_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    if buf.len() as u64 > MAX_BYTES {
        return Err("larger than 1 MiB, skipped".into());
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// The checks for every workflow file under `root`.
pub fn scan(root: &Path) -> Vec<Check> {
    let mut checks = Vec::new();
    let mut seen = 0usize;
    let mut flagged = 0usize;
    'dirs: for dir in DIRS {
        let mut path = root.to_path_buf();
        let mut real = true;
        for part in dir.split('/') {
            path.push(part);
            if !is_real_dir(&path) {
                real = false;
                break;
            }
        }
        if !real {
            continue;
        }
        let Ok(rd) = list_dir(&path) else {
            continue;
        };
        let mut names: Vec<String> = rd
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".yml") || n.ends_with(".yaml"))
            .collect();
        names.sort();
        for name in names {
            if seen >= MAX_FILES {
                checks.push(Check::info(
                    "workflows",
                    format!("stopped after {MAX_FILES} files"),
                ));
                break 'dirs;
            }
            seen += 1;
            let label = format!("{dir}/{name}");
            let src = match read_bounded(&path.join(&name)) {
                Ok(s) => s,
                Err(e) => {
                    checks.push(Check::info(label, e));
                    continue;
                }
            };
            let reach = parse(&src).map_or(Push::None, |d| push_reach(&d));
            match reach {
                Push::None | Push::Shared => {}
                Push::Unshared(pats) => {
                    flagged += 1;
                    let what = if pats.is_empty() {
                        "`on: push` has no branch filter, so it names every branch".to_string()
                    } else {
                        format!("`on: push` branches not shared: {}", pats.join(", "))
                    };
                    checks.push(
                        Check::warn(label, format!("{what}; user refs emit no push event")).hint(
                            format!(
                                "limit push to {DEFAULT_BRANCH}, release/* or green/*, and use on: pull_request for the rest"
                            ),
                        ),
                    );
                }
            }
        }
    }
    if flagged == 0 {
        checks.push(Check::new(
            Level::Ok,
            "workflows",
            format!("{seen} workflow file(s); none relies on `on: push` for a non-shared branch"),
        ));
    }
    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reach(src: &str) -> Push {
        push_reach(&parse(src).unwrap())
    }

    #[test]
    fn shared_only_filters_are_quiet() {
        let y = "name: ci\non:\n  pull_request:\n  push:\n    branches: [master, 'release/*']\njobs:\n  a:\n    run: |\n      push: x\n";
        assert_eq!(reach(y), Push::Shared);
        let y = "on:\n  push:\n    branches:\n      - master\n      - green/**\n";
        assert_eq!(reach(y), Push::Shared);
    }

    #[test]
    fn unfiltered_and_other_branches_are_listed() {
        assert_eq!(reach("on: push\n"), Push::Unshared(vec![]));
        assert_eq!(reach("on: [push, pull_request]\n"), Push::Unshared(vec![]));
        assert_eq!(reach("on:\n  push:\n"), Push::Unshared(vec![]));
        assert_eq!(
            reach("on:\n  push:\n    branches: [master, 'feature/**']\n"),
            Push::Unshared(vec!["feature/**".into()])
        );
        assert!(matches!(
            reach("on:\n  push:\n    branches-ignore: [wip]\n"),
            Push::Unshared(_)
        ));
    }

    #[test]
    fn tags_only_and_pull_request_are_quiet() {
        assert_eq!(reach("on:\n  push:\n    tags: ['v*']\n"), Push::None);
        assert_eq!(reach("on: pull_request\n"), Push::None);
        assert_eq!(reach("on: {push: {branches: [master]}}\n"), Push::Shared);
    }
}
