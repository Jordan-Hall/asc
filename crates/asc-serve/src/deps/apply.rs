//! Applying a group of moves to the files of a revision.
//!
//! For each move the files that hold the package are found from what the scan read: the lockfiles and
//! pin files that list the version, and the manifests beside or below them that declare it. The
//! manifests go first (their new requirement is what a lockfile repeats), then the pin files. What was
//! written is read back with the file parsers before it is kept: a file that no longer parses, or that
//! does not list the new version, undoes the move.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use super::listing::Meta;
use super::model::{Dep, Ecosystem, dir_of, is_within};
use super::parse::{Format, parse_file};
use super::requirement::{Bump, Style, bump};
use super::rewrite::{Extra, Pin, rewrite};

/// One package and where it goes.
#[derive(Clone, Debug)]
pub struct Move {
    pub eco: Ecosystem,
    pub name: String,
    pub from: String,
    pub to: String,
    pub meta: Meta,
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

/// The command that refreshes what a rewritten pin leaves for the package manager.
fn resolver_hint(eco: Ecosystem, name: &str) -> Option<String> {
    Some(match eco {
        Ecosystem::CratesIo => format!(
            "run `cargo update -p {name}` to refresh the dependencies of the new version in Cargo.lock"
        ),
        Ecosystem::Npm => {
            "run `npm install` to refresh the dependencies of the new version in the lockfile"
                .to_owned()
        }
        Ecosystem::RubyGems => {
            "run `bundle install` to refresh the dependencies of the new version in Gemfile.lock"
                .to_owned()
        }
        Ecosystem::Go => "run `go mod tidy` to update go.sum".to_owned(),
        Ecosystem::Maven => "build once to check that the new version resolves".to_owned(),
        _ => return None,
    })
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

/// Applies `moves` to the files in `texts` (path to format and text); `deps` is what the scan found in
/// them.
pub fn apply(texts: &Texts, deps: &[Dep], moves: &[Move]) -> Applied {
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
                Err(e) => problems.push(format!("{p}: {e}")),
            }
        }
        // The files that pin the version.
        let extra = Extra {
            checksum: m.meta.checksum.clone(),
            integrity: m.meta.integrity.clone(),
            tarball: m.meta.tarball.clone(),
            requirement: requirement.clone(),
        };
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
            match rewrite(*format, &text, &pin, &extra) {
                Ok(Some(done)) => {
                    pinned_in.push(p);
                    staged.insert((*p).to_owned(), done.text);
                }
                Ok(None) => {}
                Err(e) => problems.push(format!("{p}: {e}")),
            }
        }
        // The pin files that cannot be rewritten leave the lock stale; the manifest alone still moves.
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
                                && d.version == m.to
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
            && (!pinned_in.is_empty() || matches!(m.eco, Ecosystem::Go | Ecosystem::Maven))
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
            meta: Meta::default(),
        }
    }

    const CARGO_TOML: &str =
        "[package]\nname = \"app\"\n\n[dependencies]\nserde = \"1.0.152\"\nregex = \"1.5\"\n";
    const CARGO_LOCK: &str = "version = 3\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"regex\",\n \"serde\",\n]\n\n[[package]]\nname = \"regex\"\nversion = \"1.5.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"aaaa\"\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.152\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"bbbb\"\n";

    #[test]
    fn a_cargo_project_moves_the_requirement_and_the_lock_together() {
        let (texts, deps) = load(&[("Cargo.toml", CARGO_TOML), ("Cargo.lock", CARGO_LOCK)]);
        let mut m = mv(Ecosystem::CratesIo, "serde", "1.0.152", "2.0.1");
        m.meta.checksum = Some("c".repeat(64));
        let got = apply(&texts, &deps, &[m]);
        assert_eq!(got.done, [0]);
        assert!(got.failed.is_empty(), "{:?}", got.failed);
        assert!(got.files["Cargo.toml"].contains("serde = \"2.0.1\""));
        assert!(got.files["Cargo.lock"].contains("version = \"2.0.1\""));
        assert!(got.files["Cargo.lock"].contains(&format!("checksum = \"{}\"", "c".repeat(64))));
        assert!(
            got.files["Cargo.lock"].contains("checksum = \"aaaa\""),
            "regex is untouched"
        );
        assert_eq!(got.notes.len(), 1);
        assert!(got.notes[0].contains("cargo update -p serde"));
    }

    #[test]
    fn a_requirement_that_admits_the_version_leaves_the_manifest_alone() {
        let (texts, deps) = load(&[("Cargo.toml", CARGO_TOML), ("Cargo.lock", CARGO_LOCK)]);
        let mut m = mv(Ecosystem::CratesIo, "serde", "1.0.152", "1.0.200");
        m.meta.checksum = Some("c".repeat(64));
        let got = apply(&texts, &deps, &[m]);
        assert_eq!(got.done, [0]);
        assert_eq!(got.files.keys().collect::<Vec<_>>(), ["Cargo.lock"]);
    }

    #[test]
    fn without_the_checksum_nothing_is_written_for_the_lock_and_the_manifest_move_stands() {
        let (texts, deps) = load(&[("Cargo.toml", CARGO_TOML), ("Cargo.lock", CARGO_LOCK)]);
        let got = apply(
            &texts,
            &deps,
            &[mv(Ecosystem::CratesIo, "serde", "1.0.152", "2.0.1")],
        );
        assert_eq!(got.done, [0]);
        assert_eq!(got.files.keys().collect::<Vec<_>>(), ["Cargo.toml"]);
        assert!(
            got.notes
                .iter()
                .any(|n| n.contains("checksum") && n.contains("resolver")),
            "{:?}",
            got.notes
        );
        // Where the requirement admits the version too, there is nothing to write at all.
        let got = apply(
            &texts,
            &deps,
            &[mv(Ecosystem::CratesIo, "serde", "1.0.152", "1.0.200")],
        );
        assert!(got.done.is_empty());
        assert!(got.failed[0].1.contains("checksum"), "{:?}", got.failed);
    }

    #[test]
    fn an_unsupported_lockfile_still_lets_the_manifest_move_and_says_so() {
        let pkg = "{\n  \"dependencies\": {\n    \"lodash\": \"^4.17.20\"\n  }\n}\n";
        let yarn = "lodash@^4.17.20:\n  version \"4.17.20\"\n  resolved \"https://registry.yarnpkg.com/lodash/-/lodash-4.17.20.tgz#x\"\n";
        let (texts, deps) = load(&[("package.json", pkg), ("yarn.lock", yarn)]);
        let got = apply(
            &texts,
            &deps,
            &[mv(Ecosystem::Npm, "lodash", "4.17.20", "5.0.0")],
        );
        assert_eq!(got.done, [0]);
        assert_eq!(got.files.keys().collect::<Vec<_>>(), ["package.json"]);
        assert!(got.files["package.json"].contains("\"^5.0.0\""));
        assert!(
            got.notes
                .iter()
                .any(|n| n.contains("yarn.lock") && n.contains("resolver")),
            "{:?}",
            got.notes
        );
        // A patch the requirement admits changes nothing a resolver-less edit can change.
        let got = apply(
            &texts,
            &deps,
            &[mv(Ecosystem::Npm, "lodash", "4.17.20", "4.17.21")],
        );
        assert!(got.done.is_empty());
        assert!(got.failed[0].1.contains("yarn.lock"), "{:?}", got.failed);
    }

    #[test]
    fn a_workspace_member_moves_with_the_lockfile_above_it() {
        let root = "{\n  \"lockfileVersion\": 3,\n  \"packages\": {\n    \"\": {},\n    \"packages/a\": {\"dependencies\": {\"left-pad\": \"1.0.0\"}},\n    \"node_modules/left-pad\": {\"version\": \"1.0.0\", \"resolved\": \"u1\", \"integrity\": \"i1\"}\n  }\n}\n";
        let member = "{\n  \"dependencies\": {\n    \"left-pad\": \"1.0.0\"\n  }\n}\n";
        let other = "{\n  \"dependencies\": {\n    \"left-pad\": \"0.5.0\"\n  }\n}\n";
        let (texts, deps) = load(&[
            ("package-lock.json", root),
            ("packages/a/package.json", member),
            ("elsewhere/package.json", other),
        ]);
        let mut m = mv(Ecosystem::Npm, "left-pad", "1.0.0", "1.3.0");
        m.meta.integrity = Some("sha512-i2".into());
        m.meta.tarball = Some("https://r.example/u2".into());
        let got = apply(&texts, &deps, &[m]);
        assert_eq!(got.done, [0], "{:?}", got.failed);
        assert!(got.files["packages/a/package.json"].contains("\"1.3.0\""));
        assert!(got.files["package-lock.json"].contains("https://r.example/u2"));
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
    fn go_and_maven_notes_name_what_a_person_still_runs() {
        let gomod = "module example.com/app\n\ngo 1.21\n\nrequire github.com/x/y v1.0.0\n";
        let (texts, deps) = load(&[("go.mod", gomod)]);
        let got = apply(
            &texts,
            &deps,
            &[mv(Ecosystem::Go, "github.com/x/y", "v1.0.0", "v1.2.0")],
        );
        assert_eq!(got.done, [0]);
        assert!(
            got.notes.iter().any(|n| n.contains("go mod tidy")),
            "{:?}",
            got.notes
        );
    }
}
