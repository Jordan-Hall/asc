//! `%(describe[:options])`: the nearest tag of a commit, like `git describe`.

use std::collections::HashMap;

use asc_core::id::ChangesetId;
use asc_engine::graph::LogQuery;
use asc_engine::history::HistoryApi;
use regex::Regex;

use super::refs;
use crate::cli::common::{Result, WsCtx};

/// Ancestors examined while looking for a tag.
const SCAN: usize = 10_000;
/// Tags weighed against each other (git's `--candidates` default).
const CANDIDATES: usize = 10;

struct Tag {
    name: String,
    annotated: bool,
}

/// The tags of the repository, by the commit they point at.
pub struct Describer {
    by_commit: HashMap<ChangesetId, Vec<Tag>>,
    abbrev: usize,
}

struct Options {
    lightweight: bool,
    abbrev: usize,
    matches: Vec<Regex>,
    excludes: Vec<Regex>,
}

/// A shell glob as an anchored regular expression.
fn glob(pattern: &str) -> Option<Regex> {
    let mut re = String::from("^");
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            '[' => {
                re.push('[');
                for b in chars.by_ref() {
                    re.push(b);
                    if b == ']' {
                        break;
                    }
                }
            }
            other => re.push_str(&regex::escape(&other.to_string())),
        }
    }
    re.push('$');
    Regex::new(&re).ok()
}

impl Options {
    fn parse(text: &str, default_abbrev: usize) -> Self {
        let mut o = Self { lightweight: false, abbrev: default_abbrev, matches: Vec::new(), excludes: Vec::new() };
        for item in text.split(',').filter(|i| !i.is_empty()) {
            let (name, value) = item.split_once('=').map_or((item, None), |(n, v)| (n, Some(v)));
            match (name, value) {
                ("tags", v) => o.lightweight = v.is_none_or(|v| !matches!(v, "false" | "no" | "off" | "0")),
                ("abbrev", Some(v)) => o.abbrev = v.parse().unwrap_or(default_abbrev),
                ("match", Some(v)) => o.matches.extend(glob(v)),
                ("exclude", Some(v)) => o.excludes.extend(glob(v)),
                _ => {}
            }
        }
        o
    }

    fn wants(&self, tag: &Tag) -> bool {
        (tag.annotated || self.lightweight)
            && (self.matches.is_empty() || self.matches.iter().any(|m| m.is_match(&tag.name)))
            && !self.excludes.iter().any(|m| m.is_match(&tag.name))
    }
}

impl Describer {
    pub async fn load(ws: &WsCtx, default_abbrev: usize) -> Result<Self> {
        let mut by_commit: HashMap<ChangesetId, Vec<Tag>> = HashMap::new();
        for tip in refs::tips(ws, "refs/tags/").await? {
            let name = tip.name.strip_prefix("refs/tags/").unwrap_or(&tip.name).to_owned();
            by_commit.entry(tip.commit).or_default().push(Tag { name, annotated: tip.tag.is_some() });
        }
        for tags in by_commit.values_mut() {
            tags.sort_by(|a, b| b.annotated.cmp(&a.annotated).then_with(|| a.name.cmp(&b.name)));
        }
        Ok(Self { by_commit, abbrev: default_abbrev })
    }

    /// The description of `commit` (whose git id is `git_hex`) for `options`; empty when no tag
    /// is reachable.
    pub async fn describe(&self, hist: &dyn HistoryApi, commit: ChangesetId, git_hex: &str, options: &str) -> String {
        let opts = Options::parse(options, self.abbrev);
        let query = LogQuery { include: vec![commit], limit: Some(SCAN), ..LogQuery::default() };
        let Ok(page) = hist.log(&query, None).await else { return String::new() };
        let mut found: Vec<(ChangesetId, &Tag)> = Vec::new();
        for e in &page.entries {
            if let Some(tag) = self.by_commit.get(&e.id).and_then(|ts| ts.iter().find(|t| opts.wants(t))) {
                found.push((e.id, tag));
                if found.len() == CANDIDATES {
                    break;
                }
            }
        }
        let mut best: Option<(usize, &Tag)> = None;
        for (id, tag) in found {
            let depth = if id == commit {
                0
            } else {
                let q = LogQuery { include: vec![commit], exclude: vec![id], limit: Some(SCAN), ..LogQuery::default() };
                match hist.log(&q, None).await {
                    Ok(p) => p.entries.len(),
                    Err(_) => continue,
                }
            };
            if best.is_none_or(|(d, _)| depth < d) {
                best = Some((depth, tag));
            }
        }
        match best {
            None => String::new(),
            Some((0, tag)) => tag.name.clone(),
            Some((_, tag)) if opts.abbrev == 0 => tag.name.clone(),
            Some((depth, tag)) => format!("{}-{depth}-g{}", tag.name, &git_hex[..opts.abbrev.clamp(4, 40)]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_anchor_and_escape() {
        assert!(glob("v1.*").unwrap().is_match("v1.2"));
        assert!(!glob("v1.*").unwrap().is_match("xv1.2"));
        assert!(!glob("v1.0").unwrap().is_match("v1x0"));
        assert!(glob("v[0-9]").unwrap().is_match("v7"));
    }

    #[test]
    fn option_parsing() {
        let o = Options::parse("tags,abbrev=9,match=v*", 7);
        assert!(o.lightweight);
        assert_eq!(o.abbrev, 9);
        assert!(o.wants(&Tag { name: "v1".into(), annotated: false }));
        assert!(!o.wants(&Tag { name: "x1".into(), annotated: true }));
        assert!(!Options::parse("", 7).wants(&Tag { name: "v1".into(), annotated: false }));
    }
}
