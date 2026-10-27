//! Reading manifests and lockfiles, and nothing else of them.
//!
//! A file is recognised by its name ([`Format::of`]) and read by a pure function of its bytes: no file
//! is opened, no program runs, no path or URL in it is followed. Every reader is bounded: the caller caps
//! the size of the input ([`MAX_FILE_BYTES`]), the JSON and TOML readers cap their own nesting, the YAML
//! reader is the restricted one of [`super::yaml`], and a file holds at most [`MAX_DEPS`] dependencies.
//! A file that cannot be read whole is an error, never a partial list; an entry that is not a registry
//! package (a path, a git source, a workspace member, an unresolved property) is left out and counted in
//! the notes.
//!
//! [`combine`] joins the files of a repository: a manifest requirement is dropped when a lockfile of the
//! same ecosystem in its directory or above pins the package, the lockfile entry becomes direct, and the
//! files that only repeat another file (`go.sum` next to `go.mod`) are ignored.

mod cargo;
mod dart;
mod dotnet;
mod elixir;
mod golang;
mod jvm;
mod npm;
mod php;
mod pods;
mod python;
mod ruby;
mod swift;
mod xml;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::model::{Dep, DepKind, Ecosystem, base_of, dir_of, is_within};

/// Largest file the scanner reads.
pub const MAX_FILE_BYTES: usize = 32 << 20;
/// Most dependencies one file may hold.
pub const MAX_DEPS: usize = 200_000;
/// Most notes one file keeps.
pub const MAX_NOTES: usize = 20;

/// A file format the scanner reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Format {
    CargoLock,
    CargoToml,
    PackageLock,
    YarnLock,
    PnpmLock,
    PackageJson,
    Requirements,
    PoetryLock,
    UvLock,
    PipfileLock,
    PyProject,
    Pipfile,
    GoMod,
    GoSum,
    PomXml,
    GradleLock,
    NugetLock,
    Csproj,
    ComposerLock,
    ComposerJson,
    GemfileLock,
    Gemfile,
    MixLock,
    MixExs,
    PubspecLock,
    PackageResolved,
    PackageSwift,
    PodfileLock,
}

impl Format {
    /// The format of a file called `base`, or `None` when the scanner does not read it.
    pub fn of(base: &str) -> Option<Self> {
        Some(match base {
            "Cargo.lock" => Self::CargoLock,
            "Cargo.toml" => Self::CargoToml,
            "package-lock.json" | "npm-shrinkwrap.json" => Self::PackageLock,
            "yarn.lock" => Self::YarnLock,
            "pnpm-lock.yaml" => Self::PnpmLock,
            "package.json" => Self::PackageJson,
            "poetry.lock" => Self::PoetryLock,
            "uv.lock" => Self::UvLock,
            "Pipfile.lock" => Self::PipfileLock,
            "pyproject.toml" => Self::PyProject,
            "Pipfile" => Self::Pipfile,
            "go.mod" => Self::GoMod,
            "go.sum" => Self::GoSum,
            "pom.xml" => Self::PomXml,
            "gradle.lockfile" => Self::GradleLock,
            "packages.lock.json" => Self::NugetLock,
            "composer.lock" => Self::ComposerLock,
            "composer.json" => Self::ComposerJson,
            "Gemfile.lock" => Self::GemfileLock,
            "Gemfile" => Self::Gemfile,
            "mix.lock" => Self::MixLock,
            "mix.exs" => Self::MixExs,
            "pubspec.lock" => Self::PubspecLock,
            "Package.resolved" => Self::PackageResolved,
            "Package.swift" => Self::PackageSwift,
            "Podfile.lock" => Self::PodfileLock,
            b if b.starts_with("requirements") && b.ends_with(".txt") => Self::Requirements,
            b if [".csproj", ".fsproj", ".vbproj"]
                .iter()
                .any(|e| b.ends_with(e)) =>
            {
                Self::Csproj
            }
            _ => return None,
        })
    }

    /// The name to show: the file name, or the pattern for a family of names.
    pub fn name(self) -> &'static str {
        match self {
            Self::CargoLock => "Cargo.lock",
            Self::CargoToml => "Cargo.toml",
            Self::PackageLock => "package-lock.json",
            Self::YarnLock => "yarn.lock",
            Self::PnpmLock => "pnpm-lock.yaml",
            Self::PackageJson => "package.json",
            Self::Requirements => "requirements.txt",
            Self::PoetryLock => "poetry.lock",
            Self::UvLock => "uv.lock",
            Self::PipfileLock => "Pipfile.lock",
            Self::PyProject => "pyproject.toml",
            Self::Pipfile => "Pipfile",
            Self::GoMod => "go.mod",
            Self::GoSum => "go.sum",
            Self::PomXml => "pom.xml",
            Self::GradleLock => "gradle.lockfile",
            Self::NugetLock => "packages.lock.json",
            Self::Csproj => "*.csproj",
            Self::ComposerLock => "composer.lock",
            Self::ComposerJson => "composer.json",
            Self::GemfileLock => "Gemfile.lock",
            Self::Gemfile => "Gemfile",
            Self::MixLock => "mix.lock",
            Self::MixExs => "mix.exs",
            Self::PubspecLock => "pubspec.lock",
            Self::PackageResolved => "Package.resolved",
            Self::PackageSwift => "Package.swift",
            Self::PodfileLock => "Podfile.lock",
        }
    }

    pub fn ecosystem(self) -> Ecosystem {
        match self {
            Self::CargoLock | Self::CargoToml => Ecosystem::CratesIo,
            Self::PackageLock | Self::YarnLock | Self::PnpmLock | Self::PackageJson => {
                Ecosystem::Npm
            }
            Self::Requirements
            | Self::PoetryLock
            | Self::UvLock
            | Self::PipfileLock
            | Self::PyProject
            | Self::Pipfile => Ecosystem::PyPi,
            Self::GoMod | Self::GoSum => Ecosystem::Go,
            Self::PomXml | Self::GradleLock => Ecosystem::Maven,
            Self::NugetLock | Self::Csproj => Ecosystem::NuGet,
            Self::ComposerLock | Self::ComposerJson => Ecosystem::Packagist,
            Self::GemfileLock | Self::Gemfile => Ecosystem::RubyGems,
            Self::MixLock | Self::MixExs => Ecosystem::Hex,
            Self::PubspecLock => Ecosystem::Pub,
            Self::PackageResolved | Self::PackageSwift => Ecosystem::SwiftUrl,
            Self::PodfileLock => Ecosystem::CocoaPods,
        }
    }
}

/// What reading one file gave.
#[derive(Clone, Debug)]
pub struct Parsed {
    pub format: Format,
    pub path: String,
    pub deps: Vec<Dep>,
    /// Entries left out and why, at most [`MAX_NOTES`].
    pub notes: Vec<String>,
}

impl Parsed {
    pub(crate) fn new(format: Format, path: &str) -> Self {
        Self {
            format,
            path: path.to_owned(),
            deps: Vec::new(),
            notes: Vec::new(),
        }
    }

    pub(crate) fn note(&mut self, s: impl Into<String>) {
        if self.notes.len() < MAX_NOTES {
            self.notes.push(s.into());
        }
    }

    pub(crate) fn push(&mut self, d: Dep) -> Result<(), String> {
        if self.deps.len() >= MAX_DEPS {
            return Err(format!("the file lists more than {MAX_DEPS} dependencies"));
        }
        self.deps.push(d);
        Ok(())
    }

    /// Adds a pin unless the same name and version is already there; a direct duplicate makes the first
    /// one direct.
    pub(crate) fn pin(&mut self, d: Dep) -> Result<(), String> {
        if let Some(have) = self
            .deps
            .iter_mut()
            .rev()
            .take(64)
            .find(|h| h.ecosystem == d.ecosystem && h.version == d.version && h.name == d.name)
        {
            have.direct |= d.direct;
            return Ok(());
        }
        self.push(d)
    }
}

/// Reads `bytes` as the file at `path`, or `None` when `path` is no file the scanner reads.
pub fn parse_file(path: &str, bytes: &[u8]) -> Option<Result<Parsed, String>> {
    let format = Format::of(base_of(path))?;
    if bytes.len() > MAX_FILE_BYTES {
        return Some(Err(format!(
            "the file is larger than {MAX_FILE_BYTES} bytes"
        )));
    }
    Some(read(format, path, bytes))
}

fn read(format: Format, path: &str, bytes: &[u8]) -> Result<Parsed, String> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let text = std::str::from_utf8(bytes).map_err(|_| "the file is not UTF-8".to_owned())?;
    let mut p = Parsed::new(format, path);
    match format {
        Format::CargoLock => cargo::lock(text, &mut p)?,
        Format::CargoToml => cargo::manifest(text, &mut p)?,
        Format::PackageLock => npm::package_lock(text, &mut p)?,
        Format::YarnLock => npm::yarn_lock(text, &mut p)?,
        Format::PnpmLock => npm::pnpm_lock(text, &mut p)?,
        Format::PackageJson => npm::package_json(text, &mut p)?,
        Format::Requirements => python::requirements(text, &mut p)?,
        Format::PoetryLock => python::poetry_lock(text, &mut p)?,
        Format::UvLock => python::uv_lock(text, &mut p)?,
        Format::PipfileLock => python::pipfile_lock(text, &mut p)?,
        Format::PyProject => python::pyproject(text, &mut p)?,
        Format::Pipfile => python::pipfile(text, &mut p)?,
        Format::GoMod => golang::go_mod(text, &mut p)?,
        Format::GoSum => golang::go_sum(text, &mut p)?,
        Format::PomXml => jvm::pom(text, &mut p)?,
        Format::GradleLock => jvm::gradle_lock(text, &mut p)?,
        Format::NugetLock => dotnet::packages_lock(text, &mut p)?,
        Format::Csproj => dotnet::csproj(text, &mut p)?,
        Format::ComposerLock => php::composer_lock(text, &mut p)?,
        Format::ComposerJson => php::composer_json(text, &mut p)?,
        Format::GemfileLock => ruby::gemfile_lock(text, &mut p)?,
        Format::Gemfile => ruby::gemfile(text, &mut p)?,
        Format::MixLock => elixir::mix_lock(text, &mut p)?,
        Format::MixExs => elixir::mix_exs(text, &mut p)?,
        Format::PubspecLock => dart::pubspec_lock(text, &mut p)?,
        Format::PackageResolved => swift::package_resolved(text, &mut p)?,
        Format::PackageSwift => swift::package_swift(text, &mut p)?,
        Format::PodfileLock => pods::podfile_lock(text, &mut p)?,
    }
    Ok(p)
}

/// The dependencies of a repository after joining its files, in file and name order.
#[derive(Clone, Debug, Default)]
pub struct Combined {
    pub deps: Vec<Dep>,
}

/// Joins the parsed files of a repository (see the module documentation).
pub fn combine(files: &[Parsed]) -> Combined {
    // Files that repeat another file are dropped: go.sum lists every version ever resolved, go.mod the
    // ones in force.
    let go_mod_dirs: BTreeSet<&str> = files
        .iter()
        .filter(|f| f.format == Format::GoMod)
        .map(|f| dir_of(&f.path))
        .collect();
    let used = |f: &&Parsed| !(f.format == Format::GoSum && go_mod_dirs.contains(dir_of(&f.path)));

    // The pins, with the directory and ecosystem they hold for.
    let mut locks: Vec<Dep> = Vec::new();
    let mut manifests: Vec<Dep> = Vec::new();
    for f in files.iter().filter(used) {
        for d in &f.deps {
            match d.kind {
                DepKind::Lock => locks.push(d.clone()),
                DepKind::Manifest => manifests.push(d.clone()),
            }
        }
    }
    // name -> (lock file dirs) per ecosystem, to find a lockfile above a manifest.
    let mut lock_dirs: HashMap<(Ecosystem, String), Vec<String>> = HashMap::new();
    for d in &locks {
        lock_dirs
            .entry((d.ecosystem, d.ecosystem.canonical(&d.name)))
            .or_default()
            .push(dir_of(&d.path).to_owned());
    }
    // The names a manifest makes direct, per ecosystem and directory.
    let mut direct: BTreeMap<(Ecosystem, String), BTreeSet<String>> = BTreeMap::new();
    for d in &manifests {
        direct
            .entry((d.ecosystem, dir_of(&d.path).to_owned()))
            .or_default()
            .insert(d.ecosystem.canonical(&d.name));
    }
    for d in &mut locks {
        if d.direct {
            continue;
        }
        let name = d.ecosystem.canonical(&d.name);
        let dir = dir_of(&d.path);
        // A manifest in the directory of the lockfile or below it names the package.
        d.direct = direct.iter().any(|((eco, mdir), names)| {
            *eco == d.ecosystem && is_within(mdir, dir) && names.contains(&name)
        });
    }
    // A manifest requirement stays only when no lockfile at or above its directory pins the package.
    let mut out = locks;
    for m in manifests {
        if m.version.is_empty() {
            continue;
        }
        let name = m.ecosystem.canonical(&m.name);
        let mdir = dir_of(&m.path);
        let pinned = lock_dirs
            .get(&(m.ecosystem, name))
            .is_some_and(|dirs| dirs.iter().any(|d| is_within(mdir, d)));
        if !pinned {
            out.push(m);
        }
    }
    out.sort_by_cached_key(|d| {
        (
            d.path.clone(),
            d.ecosystem,
            d.ecosystem.canonical(&d.name),
            d.version.clone(),
        )
    });
    // The same package listed twice in one file (a lockfile repeats a version for every copy of it)
    // is one dependency, direct if any listing is.
    let mut merged: Vec<Dep> = Vec::with_capacity(out.len());
    for d in out {
        match merged.last_mut() {
            Some(last) if last.path == d.path && last.key() == d.key() => last.direct |= d.direct,
            _ => merged.push(d),
        }
    }
    Combined { deps: merged }
}

#[cfg(test)]
mod tests;
