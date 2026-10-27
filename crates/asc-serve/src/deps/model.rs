//! What the scanner talks about: ecosystems, dependencies and advisories.
//!
//! Everything a manifest or lockfile says is reduced to a [`Dep`]: the OSV ecosystem, the package name as
//! the registry spells it, the version, whether a manifest of the repository names it, and the file it came
//! from. Two dependencies are the same when [`Dep::key`] is.

use std::fmt;

use asc_core::policy::DepSeverity;

use super::version::Scheme;

/// A package ecosystem, named as OSV names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Ecosystem {
    CratesIo,
    Npm,
    PyPi,
    Go,
    Maven,
    NuGet,
    Packagist,
    RubyGems,
    Hex,
    Pub,
    SwiftUrl,
    CocoaPods,
}

impl Ecosystem {
    pub const ALL: [Ecosystem; 12] = [
        Self::CratesIo,
        Self::Npm,
        Self::PyPi,
        Self::Go,
        Self::Maven,
        Self::NuGet,
        Self::Packagist,
        Self::RubyGems,
        Self::Hex,
        Self::Pub,
        Self::SwiftUrl,
        Self::CocoaPods,
    ];

    /// The name OSV uses (`"crates.io"`, `"PyPI"`, `"Go"`, ...).
    pub fn osv_name(self) -> &'static str {
        match self {
            Self::CratesIo => "crates.io",
            Self::Npm => "npm",
            Self::PyPi => "PyPI",
            Self::Go => "Go",
            Self::Maven => "Maven",
            Self::NuGet => "NuGet",
            Self::Packagist => "Packagist",
            Self::RubyGems => "RubyGems",
            Self::Hex => "Hex",
            Self::Pub => "Pub",
            Self::SwiftUrl => "SwiftURL",
            Self::CocoaPods => "CocoaPods",
        }
    }

    /// Reads an OSV name or a common alias (`cargo`, `pip`, `golang`, `composer`, ...), any case.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        Some(match s.as_str() {
            "crates.io" | "cargo" | "rust" | "crates" => Self::CratesIo,
            "npm" | "node" | "nodejs" | "yarn" | "pnpm" | "javascript" => Self::Npm,
            "pypi" | "pip" | "python" | "poetry" | "uv" | "pipenv" => Self::PyPi,
            "go" | "golang" | "gomod" => Self::Go,
            "maven" | "gradle" | "java" => Self::Maven,
            "nuget" | "dotnet" | ".net" => Self::NuGet,
            "packagist" | "composer" | "php" => Self::Packagist,
            "rubygems" | "bundler" | "ruby" | "gem" => Self::RubyGems,
            "hex" | "mix" | "elixir" => Self::Hex,
            "pub" | "dart" | "flutter" => Self::Pub,
            "swifturl" | "swift" | "swiftpm" => Self::SwiftUrl,
            "cocoapods" | "pods" => Self::CocoaPods,
            _ => return None,
        })
    }

    /// How versions of this ecosystem order.
    pub fn scheme(self) -> Scheme {
        match self {
            Self::CratesIo
            | Self::Npm
            | Self::Go
            | Self::Hex
            | Self::Pub
            | Self::SwiftUrl
            | Self::NuGet => Scheme::Semver,
            Self::PyPi => Scheme::Pep440,
            Self::Maven => Scheme::Maven,
            Self::RubyGems | Self::CocoaPods => Scheme::Gem,
            Self::Packagist => Scheme::Composer,
        }
    }

    /// Whether OSV carries advisories for the ecosystem. CocoaPods has none: its packages are counted
    /// as not covered, never as clean.
    pub fn osv_covered(self) -> bool {
        self != Self::CocoaPods
    }

    /// Whether the scanner can ask a public registry for the newest version.
    pub fn has_registry(self) -> bool {
        !matches!(self, Self::SwiftUrl | Self::CocoaPods)
    }

    /// The form of `name` two spellings of one package share: PEP 503 for PyPI, lower case for the
    /// registries that ignore case, the name itself otherwise.
    pub fn canonical(self, name: &str) -> String {
        match self {
            Self::PyPi => {
                let mut out = String::with_capacity(name.len());
                let mut dash = false;
                for c in name.chars() {
                    if matches!(c, '-' | '_' | '.') {
                        dash = true;
                    } else {
                        if dash && !out.is_empty() {
                            out.push('-');
                        }
                        dash = false;
                        out.extend(c.to_lowercase());
                    }
                }
                out
            }
            Self::Packagist | Self::NuGet => name.to_lowercase(),
            Self::CratesIo => name.replace('_', "-").to_lowercase(),
            _ => name.to_owned(),
        }
    }
}

impl fmt::Display for Ecosystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.osv_name())
    }
}

/// Where a version comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DepKind {
    /// An exact pin: a lockfile, `go.mod`, a pinned requirement.
    Lock,
    /// A requirement of a manifest; `version` is its floor.
    Manifest,
}

impl DepKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Lock => "lock",
            Self::Manifest => "manifest",
        }
    }
}

/// One dependency, normalised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dep {
    pub ecosystem: Ecosystem,
    /// As the registry spells it (`group:artifact` for Maven, the module path for Go, the URL without
    /// scheme for SwiftURL).
    pub name: String,
    pub version: String,
    /// Named by a manifest of the repository, not only reached through another package.
    pub direct: bool,
    /// The file it was read from.
    pub path: String,
    pub kind: DepKind,
    /// The requirement text of a manifest entry; empty for a pin.
    pub requirement: String,
}

impl Dep {
    pub fn lock(ecosystem: Ecosystem, name: &str, version: &str, path: &str) -> Self {
        Self {
            ecosystem,
            name: name.to_owned(),
            version: version.to_owned(),
            direct: false,
            path: path.to_owned(),
            kind: DepKind::Lock,
            requirement: String::new(),
        }
    }

    pub fn manifest(
        ecosystem: Ecosystem,
        name: &str,
        requirement: &str,
        version: &str,
        path: &str,
    ) -> Self {
        Self {
            ecosystem,
            name: name.to_owned(),
            version: version.to_owned(),
            direct: true,
            path: path.to_owned(),
            kind: DepKind::Manifest,
            requirement: requirement.to_owned(),
        }
    }

    /// The identity of the package: ecosystem, canonical name and version.
    pub fn key(&self) -> (Ecosystem, String, String) {
        (
            self.ecosystem,
            self.ecosystem.canonical(&self.name),
            self.version.clone(),
        )
    }
}

/// The directory of `path` (`""` for the root).
pub fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

/// The file name of `path`.
pub fn base_of(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, f)| f)
}

/// Whether directory `dir` is `ancestor` or lies below it.
pub fn is_within(dir: &str, ancestor: &str) -> bool {
    ancestor.is_empty()
        || dir == ancestor
        || dir
            .strip_prefix(ancestor)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// An advisory as the scanner uses it.
#[derive(Clone, Debug, PartialEq)]
pub struct Advisory {
    pub id: String,
    pub aliases: Vec<String>,
    pub summary: String,
    /// `None`: the advisory states no severity.
    pub severity: Option<DepSeverity>,
    /// The base score when the advisory states one.
    pub cvss: f64,
    /// Every fixed version the advisory names for the package, in its own order.
    pub fixed: Vec<String>,
    pub url: String,
}

impl Advisory {
    /// The smallest fixed version greater than `current`, by the ordering of `scheme`.
    pub fn fixed_after(&self, scheme: Scheme, current: &str) -> Option<String> {
        use super::version::Version;
        let cur = Version::parse(scheme, current)?;
        self.fixed
            .iter()
            .filter_map(|f| Version::parse(scheme, f).map(|v| (v, f)))
            .filter(|(v, _)| *v > cur)
            .min_by(|a, b| a.0.cmp(&b.0))
            .map(|(_, f)| f.clone())
    }
}

/// What an advisory lookup knows about one package version.
#[derive(Clone, Debug, PartialEq)]
pub enum Lookup {
    /// The advisories (possibly none) that apply: a clean answer.
    Known(Vec<Advisory>),
    /// The source could not be asked, or has no data for the ecosystem: not clean.
    Unknown(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecosystem_names_round_trip_and_aliases_resolve() {
        for e in Ecosystem::ALL {
            assert_eq!(Ecosystem::parse(e.osv_name()), Some(e), "{e}");
            assert_eq!(Ecosystem::parse(&e.osv_name().to_uppercase()), Some(e));
        }
        assert_eq!(Ecosystem::parse("cargo"), Some(Ecosystem::CratesIo));
        assert_eq!(Ecosystem::parse(" pip "), Some(Ecosystem::PyPi));
        assert_eq!(Ecosystem::parse("composer"), Some(Ecosystem::Packagist));
        assert_eq!(Ecosystem::parse("cobol"), None);
    }

    #[test]
    fn names_have_one_canonical_form() {
        let py = Ecosystem::PyPi;
        assert_eq!(
            py.canonical("Django_REST.framework"),
            "django-rest-framework"
        );
        assert_eq!(py.canonical("zope..interface"), "zope-interface");
        assert_eq!(
            Ecosystem::Packagist.canonical("Symfony/Console"),
            "symfony/console"
        );
        assert_eq!(Ecosystem::CratesIo.canonical("Serde_Json"), "serde-json");
        assert_eq!(Ecosystem::Npm.canonical("@Scope/Name"), "@Scope/Name");
    }

    #[test]
    fn paths_split_and_nest() {
        assert_eq!(dir_of("a/b/Cargo.lock"), "a/b");
        assert_eq!(dir_of("Cargo.lock"), "");
        assert_eq!(base_of("a/b/Cargo.lock"), "Cargo.lock");
        assert!(is_within("a/b", "a"));
        assert!(is_within("a", "a"));
        assert!(is_within("a", ""));
        assert!(!is_within("ab", "a"));
        assert!(!is_within("a", "a/b"));
    }
}
