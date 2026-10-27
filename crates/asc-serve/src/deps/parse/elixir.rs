//! `mix.lock` and `mix.exs`.

use std::sync::LazyLock;

use regex::Regex;

use super::Parsed;
use crate::deps::model::{Dep, Ecosystem};
use crate::deps::version::{Scheme, Version};

const ECO: Ecosystem = Ecosystem::Hex;

/// `"key": {:hex, :package, "1.2.3", ...},` One entry per line, as `mix` writes them.
static ENTRY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*"[^"]+":\s*\{:hex,\s*:([A-Za-z0-9_]+),\s*"([^"]+)""#)
        .expect("a literal pattern")
});

static DEP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\s*:([a-z][a-z0-9_]*)\s*,").expect("a literal pattern"));

pub(super) fn mix_lock(text: &str, p: &mut Parsed) -> Result<(), String> {
    let path = p.path.clone();
    let mut left_out = 0usize;
    let mut any = false;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t == "%{" || t == "}" || t.starts_with('#') {
            continue;
        }
        any = true;
        if let Some(c) = ENTRY.captures(line) {
            if Version::parse(Scheme::Semver, &c[2]).is_some() {
                p.pin(Dep::lock(ECO, &c[1], &c[2], &path))?;
            } else {
                left_out += 1;
            }
        } else if t.contains("{:git,") || t.contains("{:path,") {
            left_out += 1;
        }
    }
    if any && p.deps.is_empty() && left_out == 0 {
        return Err("not a mix.lock: no entries".into());
    }
    if left_out > 0 {
        p.note(format!(
            "{left_out} entries are git or path sources and are not checked"
        ));
    }
    Ok(())
}

/// The packages `deps/0` of a `mix.exs` names (`{:plug, "~> 1.14"}`), for what is direct. Elixir is
/// not run.
pub(super) fn mix_exs(text: &str, p: &mut Parsed) -> Result<(), String> {
    let path = p.path.clone();
    for c in DEP.captures_iter(text) {
        p.push(Dep::manifest(ECO, &c[1], "", "", &path))?;
    }
    Ok(())
}
