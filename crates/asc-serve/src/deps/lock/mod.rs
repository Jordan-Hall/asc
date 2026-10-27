//! Rewriting lockfiles that record hashes.
//!
//! A lockfile entry holds more than the version: the digest of what was downloaded, the files of a
//! release, sometimes the dependencies the package declared. Moving a pin means writing those from the
//! registry, and writing them only when that is safe. A lockfile is rewritten when
//!
//! 1. every digest written comes from the registry's metadata for exactly the new version and has passed
//!    [`super::facts`]' checks (the right length, a second source that agrees, a signature that verifies);
//! 2. the new version declares the same dependencies (and, where the file records them, the same install
//!    properties) as the one it replaces, so the rest of the lock stays what the package manager would
//!    write; a version that changes its dependencies needs the package manager's resolver and is refused
//!    with the differences named;
//! 3. every other package in the lock that requires this one still accepts the new version.
//!
//! Anything else is an error that says which of the three failed, and the update is not made for that
//! file: a lock that cannot be moved with its manifest stops the move, it does not leave the two apart.
//! The old entry is checked too: a lock whose digest for the old version is not the registry's was not
//! made from that registry, and nothing is derived from it.

use std::sync::Arc;

use super::facts::{FactKey, FactStore, Facts, key};
use super::model::Ecosystem;
use super::parse::Format;
use super::ranges::admits;
use super::rewrite::{Done, Extra, Outcome, Pin, rewrite};

pub mod cargo;
pub mod composer;
pub mod gem;
pub mod gosum;
pub mod hashes;
pub mod node;
pub mod pipfile;
pub mod pnpm;
pub mod poetry;
pub mod uv;
pub mod yarn;

/// Most packages whose own requirements are read to check one pin.
pub const MAX_REQUESTERS: usize = 200;

/// A package in the lock that depends on the pin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Requester {
    pub name: String,
    pub version: String,
    /// The requirement as the lock records it, when it does.
    pub range: Option<String>,
}

/// Everything a lock rewriter knows besides the lock's text.
pub struct LockIn<'a> {
    pub pin: &'a Pin<'a>,
    pub from: &'a Facts,
    pub to: &'a Facts,
    /// The requirement the declaring manifest now carries, when the move changed one.
    pub requirement: Option<&'a str>,
    /// The manifest beside the lock that the lock's hash is of (`pyproject.toml`, `composer.json`,
    /// `Pipfile`), as it was and as it now is.
    pub manifest: Option<(&'a str, &'a str)>,
    /// The facts of the packages that require the pin.
    pub store: &'a FactStore,
}

/// The lock formats this module rewrites.
pub fn handles(format: Format) -> bool {
    matches!(
        format,
        Format::PackageLock
            | Format::CargoLock
            | Format::GemfileLock
            | Format::YarnLock
            | Format::PnpmLock
            | Format::PoetryLock
            | Format::UvLock
            | Format::PipfileLock
            | Format::ComposerLock
            | Format::GoSum
    )
}

/// Whether the file records the install properties of a package (so that a change in them is a change in
/// the file).
fn uses_traits(format: Format) -> bool {
    matches!(
        format,
        Format::PackageLock | Format::YarnLock | Format::PnpmLock | Format::PoetryLock
    )
}

/// The packages in `text` that depend on the pin.
pub fn requesters(format: Format, text: &str, pin: &Pin<'_>) -> Result<Vec<Requester>, String> {
    match format {
        Format::PackageLock => node::package_lock_requesters(text, pin),
        Format::YarnLock => yarn::requesters(text, pin),
        Format::PnpmLock => pnpm::requesters(text, pin),
        Format::CargoLock => cargo::requesters(text, pin),
        Format::GemfileLock => gem::requesters(text, pin),
        Format::ComposerLock => composer::requesters(text, pin),
        Format::PoetryLock => poetry::requesters(text, pin),
        Format::UvLock => uv::requesters(text, pin),
        Format::PipfileLock => pipfile::requesters(text, pin),
        _ => Ok(Vec::new()),
    }
}

/// A fact to ask the registry for, and whether it needs the full verification.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Need {
    pub key: FactKey,
    /// The package as the file spells it, which is how the registry is asked.
    pub name: String,
    pub full: bool,
}

/// What the registry has to be asked before the pin can be rewritten in `text`.
pub fn needs(format: Format, text: &str, pin: &Pin<'_>) -> Result<Vec<Need>, String> {
    let mut out = vec![
        Need {
            key: key(pin.eco, pin.name, pin.from),
            name: pin.name.to_owned(),
            full: false,
        },
        Need {
            key: key(pin.eco, pin.name, pin.to),
            name: pin.name.to_owned(),
            full: true,
        },
    ];
    let reqs = requesters(format, text, pin)?;
    let unrecorded: Vec<&Requester> = reqs.iter().filter(|r| r.range.is_none()).collect();
    if unrecorded.len() > MAX_REQUESTERS {
        return Err(format!(
            "{} packages require {}; more than {MAX_REQUESTERS} cannot be checked",
            unrecorded.len(),
            pin.name
        ));
    }
    for r in unrecorded {
        out.push(Need {
            key: key(pin.eco, &r.name, &r.version),
            name: r.name.clone(),
            full: false,
        });
    }
    Ok(out)
}

/// The requirements `facts` (a requester's declarations) holds for the pin.
fn declared_ranges(eco: Ecosystem, facts: &Facts, pin_name: &str) -> Vec<String> {
    match eco {
        Ecosystem::Npm => ["dependencies", "optionalDependencies", "peerDependencies"]
            .iter()
            .filter_map(|k| facts.deps.get(&format!("{k}:{pin_name}")).cloned())
            .collect(),
        Ecosystem::CratesIo => super::facts::crate_requirements(&facts.deps, pin_name),
        Ecosystem::PyPi => {
            let want = eco.canonical(pin_name);
            facts
                .deps
                .keys()
                .filter_map(|r| pep508_spec(r, &want))
                .collect()
        }
        Ecosystem::RubyGems => facts
            .deps
            .get(&format!("runtime:{pin_name}"))
            .cloned()
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

/// The version specifier of a PEP 508 requirement string if it is for `canonical`, not for an extra.
fn pep508_spec(req: &str, canonical: &str) -> Option<String> {
    let (main, marker) = match req.split_once(';') {
        Some((m, k)) => (m.trim(), k),
        None => (req.trim(), ""),
    };
    let end = main
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
        .unwrap_or(main.len());
    if Ecosystem::PyPi.canonical(&main[..end]) != canonical {
        return None;
    }
    if marker.contains("extra") {
        return None;
    }
    let mut rest = main[end..].trim();
    if let Some(r) = rest.strip_prefix('[') {
        rest = r.split_once(']').map_or("", |(_, t)| t).trim();
    }
    Some(rest.trim_matches(['(', ')']).trim().to_owned())
}

/// Checks the pin against the packages that require it.
fn check_requesters(format: Format, text: &str, c: &LockIn<'_>) -> Result<(), String> {
    let pin = c.pin;
    let reqs = requesters(format, text, pin)?;
    if reqs.iter().filter(|r| r.range.is_none()).count() > MAX_REQUESTERS {
        return Err(format!(
            "more than {MAX_REQUESTERS} packages require {}; they cannot all be checked",
            pin.name
        ));
    }
    for r in reqs {
        let ranges: Vec<String> = match &r.range {
            Some(x) => vec![x.clone()],
            None => {
                let k = key(pin.eco, &r.name, &r.version);
                match c.store.get(&k) {
                    Some(Ok(f)) => declared_ranges(pin.eco, f, pin.name),
                    Some(Err(e)) => {
                        return Err(format!(
                            "what {} {} requires of {} could not be read ({e})",
                            r.name, r.version, pin.name
                        ));
                    }
                    None => {
                        return Err(format!(
                            "what {} {} requires of {} was not asked",
                            r.name, r.version, pin.name
                        ));
                    }
                }
            }
        };
        for range in ranges {
            match (
                admits(pin.eco, &range, pin.from),
                admits(pin.eco, &range, pin.to),
            ) {
                (Some(true), Some(true)) | (Some(false), _) => {}
                (Some(true), Some(false)) => {
                    return Err(format!(
                        "{} {} requires {} {range}, which {} does not satisfy",
                        r.name, r.version, pin.name, pin.to
                    ));
                }
                _ => {
                    return Err(format!(
                        "the requirement {range:?} of {} {} on {} is not one this code can evaluate",
                        r.name, r.version, pin.name
                    ));
                }
            }
        }
    }
    Ok(())
}

/// The first few differences between the declarations of the two versions.
fn declared_alike(format: Format, c: &LockIn<'_>) -> Result<(), String> {
    let mut diffs = c.from.differences(c.to, uses_traits(format));
    // A Composer entry is replaced whole; only its requirements matter for the rest of the lock.
    if format == Format::ComposerLock {
        diffs.retain(|d| {
            d.contains("require:") || d.contains("conflict:") || d.contains("replace:")
        });
    }
    if diffs.is_empty() {
        return Ok(());
    }
    let shown: Vec<String> = diffs.iter().take(3).cloned().collect();
    Err(format!(
        "{} {} declares different dependencies than {} ({}{}); a resolver has to lock them",
        c.pin.name,
        c.pin.to,
        c.pin.from,
        shown.join("; "),
        if diffs.len() > 3 {
            format!("; and {} more", diffs.len() - 3)
        } else {
            String::new()
        }
    ))
}

/// The `Extra` the text-level rewriters take, from verified facts.
fn extra_of(c: &LockIn<'_>) -> Extra {
    Extra {
        checksum: c.to.sha256.clone(),
        integrity: c.to.integrity.clone(),
        tarball: c.to.tarball.clone(),
        requirement: c.requirement.map(str::to_owned),
        from_checksum: c.from.sha256.clone(),
        from_integrity: c.from.integrity.clone(),
        from_shasum: c.from.shasum.clone(),
        from_tarball: c.from.tarball.clone(),
    }
}

/// Rewrites the pin in the lockfile `text` of `format`, after the three checks.
pub fn rewrite_lock(format: Format, text: &str, c: &LockIn<'_>) -> Outcome {
    if !handles(format) {
        return Err(format!("{} is not rewritten", format.name()));
    }
    declared_alike(format, c)?;
    check_requesters(format, text, c)?;
    match format {
        Format::PackageLock | Format::CargoLock | Format::GemfileLock => {
            rewrite(format, text, c.pin, &extra_of(c))
        }
        Format::YarnLock => yarn::rewrite(text, c),
        Format::PnpmLock => pnpm::rewrite(text, c),
        Format::PoetryLock => poetry::rewrite(text, c),
        Format::UvLock => uv::rewrite(text, c),
        Format::PipfileLock => pipfile::rewrite(text, c),
        Format::ComposerLock => composer::rewrite(text, c),
        Format::GoSum => gosum::rewrite(text, c),
        _ => Err(format!("{} is not rewritten", format.name())),
    }
}

/// A `Done` for a changed text, `None` for an unchanged one.
pub(super) fn changed(old: &str, new: String) -> Outcome {
    Ok((old != new).then_some(Done {
        text: new,
        requirement: None,
    }))
}

/// Builds a [`FactStore`] from facts.
pub fn store_of(items: impl IntoIterator<Item = (FactKey, Facts)>) -> FactStore {
    items
        .into_iter()
        .map(|(k, f)| (k, Ok(Arc::new(f))))
        .collect()
}
