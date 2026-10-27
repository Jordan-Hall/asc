//! `Gemfile.lock` and `Gemfile`.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;

use super::Parsed;
use crate::deps::model::{Dep, Ecosystem};
use crate::deps::version::{Scheme, Version};

const ECO: Ecosystem = Ecosystem::RubyGems;

/// A gem in a lockfile: `name (version)`; a dependency line has a requirement in the parentheses.
static SPEC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([A-Za-z0-9_.\-]+)(?:\s+\(([^)]*)\))?!?$").expect("a literal pattern")
});

/// The sections a lockfile may hold; one of them has to be there.
const SECTIONS: [&str; 10] = [
    "GEM",
    "GIT",
    "PATH",
    "PLUGIN SOURCE",
    "PLATFORMS",
    "DEPENDENCIES",
    "RUBY VERSION",
    "BUNDLED WITH",
    "CHECKSUMS",
    "SPEC CHECKSUMS",
];

static GEM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*gem\s+['"]([A-Za-z0-9_.\-]+)['"]"#).expect("a literal pattern")
});

pub(super) fn gemfile_lock(text: &str, p: &mut Parsed) -> Result<(), String> {
    let path = p.path.clone();
    let mut section = String::new();
    let mut in_specs = false;
    let mut direct: BTreeSet<String> = BTreeSet::new();
    let mut gems: Vec<(String, String)> = Vec::new();
    let mut left_out = 0usize;
    let mut saw_section = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if !line.starts_with(' ') {
            section = line.trim().to_owned();
            in_specs = false;
            saw_section |= SECTIONS.contains(&section.as_str());
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let body = line.trim();
        match section.as_str() {
            "GEM" => {
                if indent == 2 && body == "specs:" {
                    in_specs = true;
                } else if in_specs
                    && indent == 4
                    && let Some(c) = SPEC.captures(body)
                {
                    let name = c[1].to_owned();
                    // `1.13.0-x86_64-linux`: a platform suffix after the version.
                    let version = c
                        .get(2)
                        .map_or("", |m| m.as_str())
                        .split('-')
                        .next()
                        .unwrap_or("")
                        .to_owned();
                    if Version::parse(Scheme::Gem, &version).is_some() {
                        gems.push((name, version));
                    } else {
                        left_out += 1;
                    }
                }
            }
            "GIT" | "PATH" | "PLUGIN SOURCE" if indent == 4 && SPEC.is_match(body) => {
                left_out += 1;
            }
            "DEPENDENCIES" if indent == 2 => {
                if let Some(c) = SPEC.captures(body) {
                    direct.insert(c[1].to_owned());
                }
            }
            _ => {}
        }
    }
    if !saw_section {
        return Err("not a Gemfile.lock: no sections".into());
    }
    for (name, version) in gems {
        let mut d = Dep::lock(ECO, &name, &version, &path);
        d.direct = direct.contains(&name);
        p.pin(d)?;
    }
    if left_out > 0 {
        p.note(format!(
            "{left_out} gems come from git or a path and are not checked"
        ));
    }
    Ok(())
}

/// The gems a `Gemfile` names, for what is direct. No requirement is read: a Gemfile is Ruby.
pub(super) fn gemfile(text: &str, p: &mut Parsed) -> Result<(), String> {
    let path = p.path.clone();
    for line in text.lines() {
        if let Some(c) = GEM.captures(line) {
            p.push(Dep::manifest(ECO, &c[1], "", "", &path))?;
        }
    }
    Ok(())
}
