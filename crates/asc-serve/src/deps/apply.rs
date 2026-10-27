//! Applying a group of moves to the files of a revision.
//!
//! For each move the files that hold the package are found from what the scan read: the lockfiles and
//! pin files that list the version, and the manifests beside or below them that declare it. The
//! manifests go first (their new requirement is what a lockfile repeats), then the pin files; lockfiles
//! that record hashes are moved by [`super::lock`] with facts the registries gave and that were checked.
//! A manifest or such a lockfile that cannot be moved stops the move, so a repository is never left with
//! a manifest and a lock that disagree. What was written is read back with the file parsers before it is
//! kept: a file that no longer parses, or that does not list the new version, undoes the move.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use super::facts::{FactStore, key};
use super::lock::{self, LockIn, Need};
use super::model::{Dep, Ecosystem, dir_of, is_within};
use super::parse::{Format, parse_file};
use super::requirement::{Bump, Style, bump};
use super::rewrite::{Done, Extra, Pin, rewrite};

/// One package and where it goes.
#[derive(Clone, Debug)]
pub struct Move {
    pub eco: Ecosystem,
    pub name: String,
    pub from: String,
    pub to: String,
}

/// The files of a revision by path: their format and text.
pub type Texts = HashMap<String, (Format, Arc<str>)>;

/// What applying a group did.
#[derive(Debug, Default)]
pub struct Applied {
    /// Files that changed, with their new text.
    pub files: BTreeMap<String, String>,
    /// Indexes of the moves that were made.
    pub done: Vec<usize>,
    /// Indexes of the moves that were not, and why.
    pub failed: Vec<(usize, String)>,
    /// Things a person still has to do, once each.
    pub notes: Vec<String>,
}

/// Formats that declare a requirement, tried before the formats that pin a version.
const DECLARING: [Format; 6] = [
    Format::CargoToml,
    Format::PackageJson,
    Format::ComposerJson,
    Format::Gemfile,
    Format::MixExs,
    Format::PyProject,
];

/// What a person still runs after a move this code cannot finish.
fn resolver_hint(eco: Ecosystem, _name: &str) -> Option<String> {
    match eco {
        Ecosystem::Maven => Some("build once to check that the new version resolves".to_owned()),
        _ => None,
    }
}

/// How a bare version reads in the manifests of an ecosystem.
fn style_of(eco: Ecosystem) -> Option<Style> {
    Some(match eco {
        Ecosystem::CratesIo => Style::Caret,
        Ecosystem::Npm | Ecosystem::Packagist => Style::Exact,
        Ecosystem::RubyGems | Ecosystem::Hex => Style::Gem,
        Ecosystem::PyPi => Style::Pep440,
        _ => return None,
    })
}

/// Whether the manifest `text` is where `m.from` was declared: its requirement for the package admits
/// the pinned version. A manifest below the same lockfile that asks for another version is another
/// project's and is left alone. A manifest that only names the package (no requirement) counts.
fn declares_pin(path: &str, text: &str, m: &Move) -> bool {
    let Some(Ok(parsed)) = parse_file(path, text.as_bytes()) else {
        return true;
    };
    let canon = m.eco.canonical(&m.name);
    let mut found = false;
    for d in parsed
        .deps
        .iter()
        .filter(|d| d.ecosystem == m.eco && d.ecosystem.canonical(&d.name) == canon)
    {
        found = true;
        let admits = match style_of(m.eco) {
            Some(style) if !d.requirement.is_empty() => {
                !matches!(bump(style, &d.requirement, &m.from), Bump::Changed(_))
            }
            _ => true,
        };
        if admits {
            return true;
        }
    }
    !found
}

fn same_package(d: &Dep, m: &Move) -> bool {
    d.ecosystem == m.eco
        && d.ecosystem.canonical(&d.name) == m.eco.canonical(&m.name)
        && d.version == m.from
}

/// The path of `name` in the directory of `path`.
fn sibling(path: &str, name: &str) -> String {
    let dir = dir_of(path);
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// The manifest a lockfile of `format` keeps a hash of, or has to agree with.
fn manifest_of(format: Format) -> Option<&'static str> {
    match format {
        Format::ComposerLock => Some("composer.json"),
        Format::PoetryLock | Format::UvLock => Some("pyproject.toml"),
        Format::PipfileLock => Some("Pipfile"),
        _ => None,
    }
}

/// The registry facts a move needs: of the version it leaves and the one it goes to, and of what else the
/// lockfiles say requires the package. Asked for before [`apply`], which is pure.
pub fn needs(texts: &Texts, deps: &[Dep], moves: &[Move]) -> Vec<Need> {
    let mut out: Vec<Need> = Vec::new();
    for m in moves {
        let pin = Pin {
            eco: m.eco,
            name: &m.name,
            from: &m.from,
            to: &m.to,
        };
        let paths: BTreeSet<&str> = deps
            .iter()
            .filter(|d| same_package(d, m))
            .map(|d| d.path.as_str())
            .collect();
        for p in paths {
            let Some((format, text)) = texts.get(p) else {
                continue;
            };
            let mut wanted = if lock::handles(*format) {
                lock::needs(*format, text, &pin).unwrap_or_default()
            } else {
                Vec::new()
            };
            if *format == Format::GoMod && texts.contains_key(&sibling(p, "go.sum")) {
                wanted.push(Need {
                    key: key(m.eco, &m.name, &m.from),
                    name: m.name.clone(),
                    full: false,
                });
                wanted.push(Need {
                    key: key(m.eco, &m.name, &m.to),
                    name: m.name.clone(),
                    full: true,
                });
            }
            for w in wanted {
                match out.iter_mut().find(|o| o.key == w.key) {
                    Some(o) => o.full |= w.full,
                    None => out.push(w),
                }
            }
        }
    }
    out
}

/// A lockfile moved with the facts of the registry.
#[allow(clippy::too_many_arguments)]
fn lock_move(
    format: Format,
    path: &str,
    text: &str,
    m: &Move,
    pin: &Pin<'_>,
    requirement: Option<&str>,
    texts: &Texts,
    staged: &BTreeMap<String, String>,
    work: &BTreeMap<String, String>,
    facts: &FactStore,
) -> Result<Option<Done>, String> {
    let get = |version: &str| match facts.get(&key(m.eco, &m.name, version)) {
        Some(Ok(f)) => Ok(f.clone()),
        Some(Err(e)) => Err(e.clone()),
        None => Err("the registry was not asked about it".to_owned()),
    };
    let (from, to) = (get(&m.from)?, get(&m.to)?);
    let pair = manifest_of(format).and_then(|name| {
        let mp = sibling(path, name);
        let old = texts.get(&mp)?.1.to_string();
        let new = staged
            .get(&mp)
            .or_else(|| work.get(&mp))
            .cloned()
            .unwrap_or_else(|| old.clone());
        Some((old, new))
    });
    let c = LockIn {
        pin,
        from: &from,
        to: &to,
        requirement,
        manifest: pair.as_ref().map(|(o, n)| (o.as_str(), n.as_str())),
        store: facts,
    };
    lock::rewrite_lock(format, text, &c)
}

/// Applies `moves` to the files in `texts` (path to format and text); `deps` is what the scan found in
/// them and `facts` what the registries said about the versions involved.
pub fn apply(texts: &Texts, deps: &[Dep], moves: &[Move], facts: &FactStore) -> Applied {
    let mut out = Applied::default();
    let mut work: BTreeMap<String, String> = BTreeMap::new();
    let mut notes: BTreeSet<String> = BTreeSet::new();
    let text_of = |work: &BTreeMap<String, String>, path: &str| -> Option<String> {
        work.get(path)
            .cloned()
            .or_else(|| texts.get(path).map(|(_, t)| t.to_string()))
    };
    for (i, m) in moves.iter().enumerate() {
        let occurrences: Vec<&Dep> = deps.iter().filter(|d| same_package(d, m)).collect();
        if occurrences.is_empty() {
            out.failed.push((
                i,
                "the package is not at that version in any file read".into(),
            ));
            continue;
        }
        let paths: BTreeSet<&str> = occurrences.iter().map(|d| d.path.as_str()).collect();
        let dirs: BTreeSet<&str> = paths.iter().map(|p| dir_of(p)).collect();
        let pin = Pin {
            eco: m.eco,
            name: &m.name,
            from: &m.from,
            to: &m.to,
        };
        let mut staged: BTreeMap<String, String> = BTreeMap::new();
        // What stops the move: a file that cannot be moved with the rest leaves the repository
        // inconsistent, so nothing of the move is written.
        let mut blocked: Vec<String> = Vec::new();
        // What a person still has to do for a file this code does not rewrite.
        let mut problems: Vec<String> = Vec::new();
        let mut move_notes: Vec<String> = Vec::new();
        let mut requirement: Option<String> = None;

        // Manifests that declare it, at or below a directory that pins it.
        let mut declaring: Vec<&String> = texts
            .iter()
            .filter(|(p, (f, _))| {
                f.ecosystem() == m.eco
                    && DECLARING.contains(f)
                    && dirs.iter().any(|d| is_within(dir_of(p), d))
            })
            .map(|(p, _)| p)
            .collect();
        declaring.sort();
        for p in declaring {
            let Some((format, _)) = texts.get(p) else {
                continue;
            };
            let Some(text) = text_of(&work, p) else {
                continue;
            };
            if !declares_pin(p, &text, m) {
                continue;
            }
            match rewrite(*format, &text, &pin, &Extra::default()) {
                Ok(Some(done)) => {
                    requirement = requirement.or(done.requirement);
                    staged.insert(p.clone(), done.text);
                }
                Ok(None) => {}
                Err(e) => blocked.push(format!("{p}: {e}")),
            }
        }
        // The files that pin the version.
        let mut pinned_in: Vec<&str> = Vec::new();
        for p in &paths {
            let Some((format, _)) = texts.get(*p) else {
                continue;
            };
            if DECLARING.contains(format) {
                continue;
            }
            let Some(text) = staged.get(*p).cloned().or_else(|| text_of(&work, p)) else {
                continue;
            };
            if lock::handles(*format) {
                match lock_move(
                    *format,
                    p,
                    &text,
                    m,
                    &pin,
                    requirement.as_deref(),
                    texts,
                    &staged,
                    &work,
                    facts,
                ) {
                    Ok(Some(done)) => {
                        pinned_in.push(p);
                        staged.insert((*p).to_owned(), done.text);
                        if let Some(Ok(to)) = facts.get(&key(m.eco, &m.name, &m.to)) {
                            for v in &to.verified {
                                move_notes.push(format!("{} {}: {v}", m.name, m.to));
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => blocked.push(format!("{p}: {e}")),
                }
                continue;
            }
            match rewrite(*format, &text, &pin, &Extra::default()) {
                Ok(Some(done)) => {
                    pinned_in.push(p);
                    staged.insert((*p).to_owned(), done.text);
                }
                Ok(None) => {}
                Err(e) => problems.push(format!("{p}: {e}")),
            }
            // `go.sum` beside `go.mod` holds the hashes of the module graph.
            if *format == Format::GoMod {
                let sum = sibling(p, "go.sum");
                if let Some(sum_text) = staged.get(&sum).cloned().or_else(|| text_of(&work, &sum)) {
                    match lock_move(
                        Format::GoSum,
                        &sum,
                        &sum_text,
                        m,
                        &pin,
                        None,
                        texts,
                        &staged,
                        &work,
                        facts,
                    ) {
                        Ok(Some(done)) => {
                            staged.insert(sum, done.text);
                            if let Some(Ok(to)) = facts.get(&key(m.eco, &m.name, &m.to)) {
                                for v in &to.verified {
                                    move_notes.push(format!("{} {}: {v}", m.name, m.to));
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(e) => blocked.push(format!("{sum}: {e}")),
                    }
                }
            }
        }
        if !blocked.is_empty() {
            out.failed.push((i, blocked.join("; ")));
            continue;
        }
        // A file that cannot be rewritten leaves its lock stale; the rest of the move still stands.
        if staged.is_empty() {
            let why = if problems.is_empty() {
                "no file needs to change".to_owned()
            } else {
                problems.join("; ")
            };
            out.failed.push((i, why));
            continue;
        }
        // Read back what was written.
        let mut bad = None;
        for (p, text) in &staged {
            let Some(parsed) = parse_file(p, text.as_bytes()) else {
                continue;
            };
            match parsed {
                Err(e) => {
                    bad = Some(format!("{p} does not parse after the edit: {e}"));
                    break;
                }
                Ok(parsed) => {
                    if pinned_in.contains(&p.as_str())
                        && !parsed.deps.iter().any(|d| {
                            d.ecosystem == m.eco
                                && d.ecosystem.canonical(&d.name) == m.eco.canonical(&m.name)
                                && d.version.strip_prefix('v').unwrap_or(&d.version)
                                    == m.to.strip_prefix('v').unwrap_or(&m.to)
                        })
                    {
                        bad = Some(format!("{p} does not list {} after the edit", m.to));
                        break;
                    }
                }
            }
        }
        if let Some(why) = bad {
            out.failed.push((i, why));
            continue;
        }
        for p in problems {
            move_notes.push(format!("{p}; a resolver has to update it"));
        }
        if let Some(h) = resolver_hint(m.eco, &m.name)
            && (!pinned_in.is_empty() || matches!(m.eco, Ecosystem::Maven))
        {
            move_notes.push(h);
        }
        for n in move_notes {
            notes.insert(n);
        }
        work.extend(staged);
        out.done.push(i);
    }
    out.files = work;
    out.notes = notes.into_iter().collect();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::facts::{Facts, GoSums};
    use crate::deps::parse::{Parsed, combine};

    fn load(files: &[(&str, &str)]) -> (Texts, Vec<Dep>) {
        let mut texts = HashMap::new();
        let mut parsed: Vec<Parsed> = Vec::new();
        for (p, t) in files {
            let format = Format::of(crate::deps::model::base_of(p)).unwrap();
            texts.insert((*p).to_owned(), (format, Arc::from(*t)));
            parsed.push(parse_file(p, t.as_bytes()).unwrap().unwrap());
        }
        (texts, combine(&parsed).deps)
    }

    fn mv(eco: Ecosystem, name: &str, from: &str, to: &str) -> Move {
        Move {
            eco,
            name: name.into(),
            from: from.into(),
            to: to.into(),
        }
    }

    fn store(items: Vec<(Ecosystem, &str, &str, Facts)>) -> FactStore {
        items
            .into_iter()
            .map(|(e, n, v, f)| (key(e, n, v), Ok(Arc::new(f))))
            .collect()
    }

    fn crate_facts(sum: &str) -> Facts {
        Facts {
            sha256: Some(sum.to_owned()),
            verified: vec!["the crates.io API and index agree on the checksum".into()],
            ..Facts::default()
        }
    }

    const OLD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const NEW: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const REG: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    const CARGO_TOML: &str =
        "[package]\nname = \"app\"\n\n[dependencies]\nserde = \"1.0.152\"\nregex = \"1.5\"\n";

    fn cargo_lock() -> String {
        format!(
            "version = 3\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"regex\",\n \"serde\",\n]\n\n[[package]]\nname = \"regex\"\nversion = \"1.5.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{REG}\"\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.152\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{OLD}\"\n"
        )
    }

    fn cargo_store(to: &str) -> FactStore {
        store(vec![
            (Ecosystem::CratesIo, "serde", "1.0.152", crate_facts(OLD)),
            (Ecosystem::CratesIo, "serde", to, crate_facts(NEW)),
        ])
    }

    #[test]
    fn a_cargo_project_moves_the_requirement_and_the_lock_together() {
        let lock = cargo_lock();
        let (texts, deps) = load(&[("Cargo.toml", CARGO_TOML), ("Cargo.lock", &lock)]);
        let got = apply(
            &texts,
            &deps,
            &[mv(Ecosystem::CratesIo, "serde", "1.0.152", "2.0.1")],
            &cargo_store("2.0.1"),
        );
        assert_eq!(got.done, [0], "{:?}", got.failed);
        assert!(got.files["Cargo.toml"].contains("serde = \"2.0.1\""));
        assert!(got.files["Cargo.lock"].contains("version = \"2.0.1\""));
        assert!(got.files["Cargo.lock"].contains(&format!("checksum = \"{NEW}\"")));
        assert!(
            got.files["Cargo.lock"].contains(&format!("checksum = \"{REG}\"")),
            "regex is untouched"
        );
        // What was verified is said, and no resolver is asked for.
        assert_eq!(got.notes.len(), 1, "{:?}", got.notes);
        assert!(
            got.notes[0].contains("crates.io API and index agree"),
            "{:?}",
            got.notes
        );
    }

    #[test]
    fn a_requirement_that_admits_the_version_leaves_the_manifest_alone() {
        let lock = cargo_lock();
        let (texts, deps) = load(&[("Cargo.toml", CARGO_TOML), ("Cargo.lock", &lock)]);
        let got = apply(
            &texts,
            &deps,
            &[mv(Ecosystem::CratesIo, "serde", "1.0.152", "1.0.200")],
            &cargo_store("1.0.200"),
        );
        assert_eq!(got.done, [0]);
        assert_eq!(got.files.keys().collect::<Vec<_>>(), ["Cargo.lock"]);
    }

    #[test]
    fn a_lock_that_cannot_be_moved_stops_the_move_and_leaves_the_manifest_alone() {
        let lock = cargo_lock();
        let (texts, deps) = load(&[("Cargo.toml", CARGO_TOML), ("Cargo.lock", &lock)]);
        let m = [mv(Ecosystem::CratesIo, "serde", "1.0.152", "2.0.1")];
        // The registry was not asked.
        let got = apply(&texts, &deps, &m, &FactStore::new());
        assert!(got.done.is_empty());
        assert!(got.files.is_empty(), "no manifest edit without its lock");
        assert!(
            got.failed[0].1.contains("Cargo.lock") && got.failed[0].1.contains("not asked"),
            "{:?}",
            got.failed
        );
        // The registry could not be reached.
        let mut s = cargo_store("2.0.1");
        s.insert(
            key(Ecosystem::CratesIo, "serde", "2.0.1"),
            Err("crates.io index: the request timed out".into()),
        );
        let got = apply(&texts, &deps, &m, &s);
        assert!(got.failed[0].1.contains("timed out"), "{:?}", got.failed);
        // The lock's checksum for the old version is not the registry's.
        let mut s = cargo_store("2.0.1");
        s.insert(
            key(Ecosystem::CratesIo, "serde", "1.0.152"),
            Ok(Arc::new(crate_facts(REG))),
        );
        let got = apply(&texts, &deps, &m, &s);
        assert!(
            got.failed[0]
                .1
                .contains("not the one the registry publishes"),
            "{:?}",
            got.failed
        );
        // The new version depends on something the old one did not.
        let mut s = cargo_store("2.0.1");
        let mut f = crate_facts(NEW);
        f.deps
            .insert("normal:serde_derive:".into(), "^1|||true".into());
        s.insert(key(Ecosystem::CratesIo, "serde", "2.0.1"), Ok(Arc::new(f)));
        let got = apply(&texts, &deps, &m, &s);
        assert!(
            got.failed[0].1.contains("serde_derive") && got.failed[0].1.contains("resolver"),
            "{:?}",
            got.failed
        );
        assert!(got.files.is_empty());
    }

    #[test]
    fn a_package_that_another_locked_package_pins_cannot_move_past_it() {
        let lock = format!(
            "{}\n[[package]]\nname = \"pinned\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{REG}\"\ndependencies = [\n \"serde 1.0.152\",\n]\n",
            cargo_lock()
        );
        let (texts, deps) = load(&[("Cargo.toml", CARGO_TOML), ("Cargo.lock", &lock)]);
        let mut s = cargo_store("1.0.200");
        let mut p = crate_facts(REG);
        p.deps
            .insert("normal:serde:".into(), "=1.0.152||false|true".into());
        s.insert(key(Ecosystem::CratesIo, "pinned", "1.0.0"), Ok(Arc::new(p)));
        let got = apply(
            &texts,
            &deps,
            &[mv(Ecosystem::CratesIo, "serde", "1.0.152", "1.0.200")],
            &s,
        );
        assert!(got.done.is_empty());
        assert!(
            got.failed[0].1.contains("pinned 1.0.0 requires serde"),
            "{:?}",
            got.failed
        );
        // One that allows it is fine.
        let mut p = crate_facts(REG);
        p.deps
            .insert("normal:serde:".into(), "^1.0||false|true".into());
        s.insert(key(Ecosystem::CratesIo, "pinned", "1.0.0"), Ok(Arc::new(p)));
        let got = apply(
            &texts,
            &deps,
            &[mv(Ecosystem::CratesIo, "serde", "1.0.152", "1.0.200")],
            &s,
        );
        assert_eq!(got.done, [0], "{:?}", got.failed);
    }

    #[test]
    fn what_the_registry_has_to_be_asked_is_listed_with_the_full_check_only_for_the_new_version() {
        let lock = format!(
            "{}\n[[package]]\nname = \"pinned\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{REG}\"\ndependencies = [\n \"serde 1.0.152\",\n]\n",
            cargo_lock()
        );
        let (texts, deps) = load(&[("Cargo.toml", CARGO_TOML), ("Cargo.lock", &lock)]);
        let n = needs(
            &texts,
            &deps,
            &[mv(Ecosystem::CratesIo, "serde", "1.0.152", "1.0.200")],
        );
        let find = |v: &str| {
            n.iter()
                .find(|x| x.key.2 == v)
                .map(|x| (x.key.1.clone(), x.full))
        };
        assert_eq!(find("1.0.152"), Some(("serde".into(), false)));
        assert_eq!(find("1.0.200"), Some(("serde".into(), true)));
        assert_eq!(
            find("1.0.0"),
            Some(("pinned".into(), false)),
            "the dependent is asked what it requires"
        );
        assert_eq!(n.len(), 3);
    }

    #[test]
    fn a_yarn_project_moves_package_json_and_yarn_lock_together() {
        let pkg = "{\n  \"dependencies\": {\n    \"lodash\": \"^4.17.20\"\n  }\n}\n";
        let sha_a = "a".repeat(40);
        let sha_b = "b".repeat(40);
        let int_a = "sha512-AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQ==";
        let int_b = "sha512-AgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAg==";
        let yarn = format!(
            "# yarn lockfile v1\n\n\nlodash@^4.17.20:\n  version \"4.17.20\"\n  resolved \"https://registry.yarnpkg.com/lodash/-/lodash-4.17.20.tgz#{sha_a}\"\n  integrity {int_a}\n"
        );
        let (texts, deps) = load(&[("package.json", pkg), ("yarn.lock", &yarn)]);
        let f = |v: &str, sha: &str, int: &str| Facts {
            tarball: Some(format!(
                "https://registry.npmjs.org/lodash/-/lodash-{v}.tgz"
            )),
            shasum: Some(sha.to_owned()),
            integrity: Some(int.to_owned()),
            ..Facts::default()
        };
        let s = store(vec![
            (
                Ecosystem::Npm,
                "lodash",
                "4.17.20",
                f("4.17.20", &sha_a, int_a),
            ),
            (Ecosystem::Npm, "lodash", "5.0.0", f("5.0.0", &sha_b, int_b)),
        ]);
        let got = apply(
            &texts,
            &deps,
            &[mv(Ecosystem::Npm, "lodash", "4.17.20", "5.0.0")],
            &s,
        );
        assert_eq!(got.done, [0], "{:?}", got.failed);
        assert!(got.files["package.json"].contains("\"^5.0.0\""));
        assert!(
            got.files["yarn.lock"].contains("lodash@^5.0.0:\n  version \"5.0.0\""),
            "{}",
            got.files["yarn.lock"]
        );
        assert!(got.files["yarn.lock"].contains(&format!("lodash-5.0.0.tgz#{sha_b}")));
        assert!(got.files["yarn.lock"].contains(&format!("integrity {int_b}")));
        // A patch the requirement admits leaves package.json alone and still moves the lock.
        let s = store(vec![
            (
                Ecosystem::Npm,
                "lodash",
                "4.17.20",
                f("4.17.20", &sha_a, int_a),
            ),
            (
                Ecosystem::Npm,
                "lodash",
                "4.17.21",
                f("4.17.21", &sha_b, int_b),
            ),
        ]);
        let got = apply(
            &texts,
            &deps,
            &[mv(Ecosystem::Npm, "lodash", "4.17.20", "4.17.21")],
            &s,
        );
        assert_eq!(got.done, [0], "{:?}", got.failed);
        assert_eq!(got.files.keys().collect::<Vec<_>>(), ["yarn.lock"]);
    }

    #[test]
    fn a_workspace_member_moves_with_the_lockfile_above_it() {
        let int_a = "sha512-AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQ==";
        let int_b = "sha512-AgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAg==";
        let root = format!(
            "{{\n  \"lockfileVersion\": 3,\n  \"packages\": {{\n    \"\": {{}},\n    \"packages/a\": {{\"dependencies\": {{\"left-pad\": \"1.0.0\"}}}},\n    \"node_modules/left-pad\": {{\"version\": \"1.0.0\", \"resolved\": \"https://r.example/left-pad-1.0.0.tgz\", \"integrity\": \"{int_a}\"}}\n  }}\n}}\n"
        );
        let member = "{\n  \"dependencies\": {\n    \"left-pad\": \"1.0.0\"\n  }\n}\n";
        let other = "{\n  \"dependencies\": {\n    \"left-pad\": \"0.5.0\"\n  }\n}\n";
        let (texts, deps) = load(&[
            ("package-lock.json", &root),
            ("packages/a/package.json", member),
            ("elsewhere/package.json", other),
        ]);
        let f = |v: &str, int: &str| Facts {
            integrity: Some(int.to_owned()),
            tarball: Some(format!("https://r.example/left-pad-{v}.tgz")),
            ..Facts::default()
        };
        let s = store(vec![
            (Ecosystem::Npm, "left-pad", "1.0.0", f("1.0.0", int_a)),
            (Ecosystem::Npm, "left-pad", "1.3.0", f("1.3.0", int_b)),
        ]);
        let got = apply(
            &texts,
            &deps,
            &[mv(Ecosystem::Npm, "left-pad", "1.0.0", "1.3.0")],
            &s,
        );
        assert_eq!(got.done, [0], "{:?}", got.failed);
        assert!(got.files["packages/a/package.json"].contains("\"1.3.0\""));
        assert!(got.files["package-lock.json"].contains("https://r.example/left-pad-1.3.0.tgz"));
        assert!(
            !got.files.contains_key("elsewhere/package.json"),
            "another project's manifest"
        );
        assert!(
            got.files["package-lock.json"].contains("\"left-pad\": \"1.3.0\""),
            "the root requirement follows"
        );
    }

    #[test]
    fn go_sum_follows_go_mod_with_the_hashes_the_checksum_database_gave() {
        let gomod = "module example.com/app\n\ngo 1.21\n\nrequire github.com/x/y v1.0.0\n";
        let gosum = "github.com/x/y v1.0.0 h1:old=\ngithub.com/x/y v1.0.0/go.mod h1:oldm=\n";
        let (texts, deps) = load(&[("go.mod", gomod), ("go.sum", gosum)]);
        let f = |zip: &str, module: &str| Facts {
            go: Some(GoSums {
                zip: zip.into(),
                module: module.into(),
            }),
            verified: vec![
                "the Go proxy's go.mod and zip hash to the checksum database's records".into(),
            ],
            ..Facts::default()
        };
        let s = store(vec![
            (Ecosystem::Go, "github.com/x/y", "v1.0.0", f("", "h1:oldm=")),
            (
                Ecosystem::Go,
                "github.com/x/y",
                "v1.2.0",
                f("h1:new=", "h1:newm="),
            ),
        ]);
        let m = [mv(Ecosystem::Go, "github.com/x/y", "v1.0.0", "v1.2.0")];
        let got = apply(&texts, &deps, &m, &s);
        assert_eq!(got.done, [0], "{:?}", got.failed);
        assert!(got.files["go.mod"].contains("github.com/x/y v1.2.0"));
        assert_eq!(
            got.files["go.sum"],
            "github.com/x/y v1.0.0/go.mod h1:oldm=\ngithub.com/x/y v1.2.0 h1:new=\ngithub.com/x/y v1.2.0/go.mod h1:newm=\n"
        );
        assert!(
            got.notes.iter().all(|n| !n.contains("go mod tidy")),
            "{:?}",
            got.notes
        );
        // Without the database's answer go.mod is not moved alone.
        let got = apply(&texts, &deps, &m, &FactStore::new());
        assert!(got.files.is_empty());
        assert!(got.failed[0].1.contains("go.sum"), "{:?}", got.failed);
        assert_eq!(needs(&texts, &deps, &m).len(), 2);
    }

    #[test]
    fn a_lockfile_with_no_registry_facts_here_leaves_the_manifest_moving_and_says_so() {
        let mx = "defp deps do\n  [\n    {:plug, \"~> 1.14\"}\n  ]\nend\n";
        let lock = "%{\n  \"plug\": {:hex, :plug, \"1.14.0\", \"abc\", [:mix], [], \"hexpm\", \"def\"},\n}\n";
        let (texts, deps) = load(&[("mix.exs", mx), ("mix.lock", lock)]);
        let got = apply(
            &texts,
            &deps,
            &[mv(Ecosystem::Hex, "plug", "1.14.0", "2.0.0")],
            &FactStore::new(),
        );
        assert_eq!(got.done, [0], "{:?}", got.failed);
        assert_eq!(got.files.keys().collect::<Vec<_>>(), ["mix.exs"]);
        assert!(
            got.notes
                .iter()
                .any(|n| n.contains("mix.lock") && n.contains("resolver")),
            "{:?}",
            got.notes
        );
    }

    #[test]
    fn several_moves_share_files_and_a_failure_does_not_stop_the_others() {
        let req = "Django==3.2.0\nrequests==2.28.1 \\\n    --hash=sha256:aaaa\nflask==2.0.0\n";
        let (texts, deps) = load(&[("requirements.txt", req)]);
        let got = apply(
            &texts,
            &deps,
            &[
                mv(Ecosystem::PyPi, "django", "3.2.0", "4.2.0"),
                mv(Ecosystem::PyPi, "requests", "2.28.1", "2.31.0"),
                mv(Ecosystem::PyPi, "flask", "2.0.0", "3.0.0"),
                mv(Ecosystem::PyPi, "ghost", "1.0.0", "2.0.0"),
            ],
            &FactStore::new(),
        );
        assert_eq!(got.done, [0, 2]);
        assert_eq!(got.failed.len(), 2);
        assert!(
            got.failed
                .iter()
                .any(|(i, w)| *i == 1 && w.contains("--hash"))
        );
        assert!(
            got.failed
                .iter()
                .any(|(i, w)| *i == 3 && w.contains("not at that version"))
        );
        assert_eq!(
            got.files["requirements.txt"],
            "Django==4.2.0\nrequests==2.28.1 \\\n    --hash=sha256:aaaa\nflask==3.0.0\n"
        );
    }

    #[test]
    fn maven_notes_name_what_a_person_still_runs() {
        let pom = "<project><dependencies><dependency><groupId>org.x</groupId><artifactId>y</artifactId><version>1.0.0</version></dependency></dependencies></project>";
        let (texts, deps) = load(&[("pom.xml", pom)]);
        let got = apply(
            &texts,
            &deps,
            &[mv(Ecosystem::Maven, "org.x:y", "1.0.0", "1.2.0")],
            &FactStore::new(),
        );
        assert_eq!(got.done, [0], "{:?}", got.failed);
        assert!(
            got.notes.iter().any(|n| n.contains("build once")),
            "{:?}",
            got.notes
        );
    }
}
