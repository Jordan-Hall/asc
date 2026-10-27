//! `Podfile.lock`.

use std::collections::BTreeSet;

use super::Parsed;
use crate::deps::model::{Dep, Ecosystem};
use crate::deps::version::{Scheme, Version};
use crate::deps::yaml::{self, Y};

const ECO: Ecosystem = Ecosystem::CocoaPods;

/// `Name (1.2.3)` into the name of the pod (a subspec `Firebase/Core` belongs to `Firebase`) and what
/// is in the parentheses.
fn split(entry: &str) -> (String, Option<String>) {
    let entry = entry.trim().trim_end_matches(':');
    let (name, rest) = match entry.split_once(" (") {
        Some((n, r)) => (n, r.strip_suffix(')').map(str::to_owned)),
        None => (entry, None),
    };
    let base = name.split('/').next().unwrap_or(name);
    (base.to_owned(), rest)
}

/// The text of a sequence item: a plain entry, or the key of a one-entry mapping.
fn item_text(y: &Y) -> Option<&str> {
    match y {
        Y::Str(s) => Some(s),
        Y::Map(m) => m.first().map(|(k, _)| k.as_str()),
        _ => None,
    }
}

pub(super) fn podfile_lock(text: &str, p: &mut Parsed) -> Result<(), String> {
    let doc = yaml::parse(text)?;
    if !matches!(doc, Y::Map(_)) {
        return Err("not a Podfile.lock".into());
    }
    let path = p.path.clone();
    let external: BTreeSet<String> = doc
        .get("EXTERNAL SOURCES")
        .map_or(&[][..], Y::entries)
        .iter()
        .map(|(k, _)| split(k).0)
        .collect();
    let direct: BTreeSet<String> = doc
        .get("DEPENDENCIES")
        .map_or(&[][..], Y::as_seq)
        .iter()
        .filter_map(item_text)
        .map(|t| split(t).0)
        .collect();
    let mut left_out = 0usize;
    for item in doc.get("PODS").map_or(&[][..], Y::as_seq) {
        let Some(t) = item_text(item) else { continue };
        let (name, version) = split(t);
        if external.contains(&name) {
            left_out += 1;
            continue;
        }
        let Some(version) = version.filter(|v| Version::parse(Scheme::Gem, v).is_some()) else {
            left_out += 1;
            continue;
        };
        let mut d = Dep::lock(ECO, &name, &version, &path);
        d.direct = direct.contains(&name);
        p.pin(d)?;
    }
    if left_out > 0 {
        p.note(format!(
            "{left_out} pods come from git or a path and are not checked"
        ));
    }
    Ok(())
}
