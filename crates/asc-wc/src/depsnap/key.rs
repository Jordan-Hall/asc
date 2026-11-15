//! The snapshot key (`docs/design/DEPENDENCY-SNAPSHOTS.md`, "The key").
//!
//! A [`SnapshotKey`] is a BLAKE3 over named, length-prefixed parts, built like the action keys of
//! `asc-buildcache` but under its own context. The key is deterministic across platforms: lengths
//! are little-endian `u64`, paths are repository-relative and `/`-separated, line endings of the
//! scrubbed configuration files are normalised, and no part depends on the machine's byte order,
//! path syntax or locale.
//!
//! Every function here is pure except [`read_lockfiles`]. Finding the files and probing the
//! machine belong to the callers; this module takes their results.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use crate::depclone::LOCKFILES;

/// The key context of snapshot keys.
pub const KEY_CONTEXT: &str = "asc deps snapshot v1";

/// The value of the `format` part: the capture and restore rules the key belongs to.
pub const FORMAT: &str = "1";

/// The names of the parts, in the order they are hashed.
pub const PART_NAMES: [&str; 15] = [
    "format",
    "ecosystem",
    "root",
    "manager",
    "manager_version",
    "lockfiles",
    "manifests",
    "config",
    "os",
    "arch",
    "libc",
    "node_abi",
    "install",
    "scripts",
    "env",
];

/// Configuration files whose credential lines are removed before hashing.
const SCRUBBED_FILES: [&str; 4] = [".npmrc", ".yarnrc", ".yarnrc.yml", "bunfig.toml"];

/// A configuration line whose key contains one of these (any case) is removed. The first eight are
/// the design's list (`_auth` covers `_authToken`); `npmauth` (Yarn's `npmAuthToken` and
/// `npmAuthIdent`), `token` and `password` cover the spellings of Yarn and Bun, which the npm-style
/// names do not.
const SECRET_KEYS: [&str; 11] = [
    "_auth",
    "_authtoken",
    "_password",
    "username",
    "email",
    "certfile",
    "keyfile",
    "cafile",
    "npmauth",
    "token",
    "password",
];

/// An environment variable whose name contains one of these (any case) is never a key part.
const SECRET_ENV: [&str; 4] = ["TOKEN", "SECRET", "PASSWORD", "AUTH"];

/// What a key cannot be built from.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyError {
    #[error("`{0}` is not a repository-relative `/`-separated path")]
    BadPath(String),
    #[error("`{0}` is not a lockfile name")]
    NotLockfile(String),
    #[error("`{0}` is not a package.json outside node_modules")]
    NotManifest(String),
    #[error("the file `{0}` is listed twice")]
    DuplicateFile(String),
    #[error("no lockfile: the install is not pinned")]
    NoLockfile,
    #[error("the root package.json is missing from the manifests")]
    NoRootManifest,
    #[error("`{0}` is not a valid {1}")]
    BadValue(String, &'static str),
    #[error("the environment variable `{0}` looks like a credential and cannot be a key part")]
    SecretEnvName(String),
    #[error("the environment variable `{0}` is listed twice")]
    DuplicateEnv(String),
    #[error("the install argv is one empty argument, which is not told apart from none")]
    EmptyArg,
    #[error("the parts are not the parts of a key: expected `{expected}` at position {at}")]
    BadParts { at: usize, expected: &'static str },
}

/// The ecosystem of the install. Other ecosystems (cargo vendor, pip, Go) add variants; each one
/// keeps its snapshots apart through this part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ecosystem {
    Npm,
}

impl Ecosystem {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Npm => "npm",
        }
    }
}

/// The package manager that ran the install.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Manager {
    Npm,
    Pnpm,
    Yarn,
    Bun,
}

impl Manager {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Yarn => "yarn",
            Self::Bun => "bun",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "npm" => Self::Npm,
            "pnpm" => Self::Pnpm,
            "yarn" => Self::Yarn,
            "bun" => Self::Bun,
            _ => return None,
        })
    }
}

/// The C library the `node` executable links.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Libc {
    Glibc {
        major: u32,
        minor: u32,
    },
    Musl,
    /// Not Linux.
    None,
}

impl fmt::Display for Libc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Glibc { major, minor } => write!(f, "glibc-{major}.{minor}"),
            Self::Musl => f.write_str("musl"),
            Self::None => f.write_str("none"),
        }
    }
}

/// The platform of the machine that ran the install, in Node's names (`process.platform`,
/// `process.arch`, `process.versions.modules`), because the `os` and `cpu` fields of packages use
/// them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Platform {
    /// `linux`, `darwin`, `win32`, `freebsd`; lowercase letters, digits and `_`.
    pub os: String,
    /// `x64`, `arm64`, `ia32`, ...; lowercase letters, digits and `_`.
    pub arch: String,
    pub libc: Libc,
    /// `NODE_MODULE_VERSION`; `None` when no `node` is on `PATH`.
    pub node_abi: Option<u32>,
}

/// Whether lifecycle scripts ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scripts {
    None,
    /// Only the listed packages' scripts ran. The order and repeats do not matter.
    Allow(Vec<String>),
    All,
}

/// One environment variable that is a key part. `None`: the variable is not set, which is not the
/// same as set to the empty string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvEntry {
    pub name: String,
    pub value: Option<String>,
}

impl EnvEntry {
    /// Reads `names` through `get`. A name that looks like a credential is refused.
    pub fn read(
        names: &[String],
        get: impl Fn(&str) -> Option<String>,
    ) -> Result<Vec<Self>, KeyError> {
        names
            .iter()
            .map(|name| {
                check_env_name(name)?;
                Ok(Self {
                    name: name.clone(),
                    value: get(name),
                })
            })
            .collect()
    }
}

/// A file that is part of a digest: its path (relative to the project root, `/`-separated) and the
/// BLAKE3 of the bytes that count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDigest {
    pub path: String,
    pub digest: [u8; 32],
}

impl FileDigest {
    /// The whole bytes of the file.
    pub fn raw(path: impl Into<String>, bytes: &[u8]) -> Self {
        Self {
            path: path.into(),
            digest: *blake3::hash(bytes).as_bytes(),
        }
    }

    /// A configuration file: the settings files `.npmrc`, `.yarnrc`, `.yarnrc.yml` and
    /// `bunfig.toml` are scrubbed ([`scrub_config`]) first; every other file (a patch, a plugin,
    /// `.pnpmfile.cjs`, `pnpm-workspace.yaml`) counts whole, because a line removed from code or a
    /// patch would hide a real change.
    pub fn config(path: impl Into<String>, bytes: &[u8]) -> Self {
        let path = path.into();
        let name = path.rsplit('/').next().unwrap_or(&path);
        let digest = if SCRUBBED_FILES.contains(&name) {
            *blake3::hash(&scrub_config(bytes)).as_bytes()
        } else {
            *blake3::hash(bytes).as_bytes()
        };
        Self { path, digest }
    }
}

/// The lockfiles of [`LOCKFILES`] that are regular files of `dir`, named without a directory. A
/// link is not followed. `bun.lockb` is binary and is hashed like the rest.
pub fn read_lockfiles(dir: &Path) -> io::Result<Vec<FileDigest>> {
    let mut out = Vec::new();
    for name in LOCKFILES {
        let path = dir.join(name);
        match fs::symlink_metadata(&path) {
            Ok(m) if m.is_file() => out.push(FileDigest::raw(name, &fs::read(&path)?)),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// The inputs of a snapshot key. [`KeyParts::parts`] gives the clear-text parts a record keeps;
/// [`KeyParts::key`] hashes them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyParts {
    pub ecosystem: Ecosystem,
    /// The project root: repository-relative, `/`-separated, `""` for the repository root.
    pub root: String,
    pub manager: Manager,
    /// The exact version, for example `9.12.0`, without a corepack hash.
    pub manager_version: String,
    /// Each [`LOCKFILES`] name present at the root. At least one.
    pub lockfiles: Vec<FileDigest>,
    /// The `package.json` of the root and of every other workspace package of the install, with
    /// paths relative to the project root.
    pub manifests: Vec<FileDigest>,
    /// The install configuration files (see [`FileDigest::config`]); may be empty.
    pub config: Vec<FileDigest>,
    pub platform: Platform,
    /// The install argv after the program name.
    pub install: Vec<String>,
    pub scripts: Scripts,
    pub env: Vec<EnvEntry>,
}

impl KeyParts {
    /// The parts, in hashing order, as a record keeps them: clear text, except that the file
    /// parts hold a digest in hex.
    pub fn parts(&self) -> Result<Vec<(String, String)>, KeyError> {
        let Self {
            ecosystem,
            root,
            manager,
            manager_version,
            lockfiles,
            manifests,
            config,
            platform,
            install,
            scripts,
            env,
        } = self;
        if !root.is_empty() {
            check_path(root)?;
        }
        check_token(manager_version, "manager version")?;
        check_token_lower(&platform.os, "os name")?;
        check_token_lower(&platform.arch, "arch name")?;

        if lockfiles.is_empty() {
            return Err(KeyError::NoLockfile);
        }
        if let Some(f) = lockfiles
            .iter()
            .find(|f| !LOCKFILES.contains(&f.path.as_str()))
        {
            return Err(KeyError::NotLockfile(f.path.clone()));
        }
        for f in manifests {
            check_path(&f.path)?;
            let in_modules = f.path.split('/').any(|c| c == "node_modules");
            if in_modules || f.path.rsplit('/').next() != Some("package.json") {
                return Err(KeyError::NotManifest(f.path.clone()));
            }
        }
        if !manifests.iter().any(|f| f.path == "package.json") {
            return Err(KeyError::NoRootManifest);
        }
        for f in config {
            check_path(&f.path)?;
        }
        if install.len() == 1 && install[0].is_empty() {
            return Err(KeyError::EmptyArg);
        }
        if let Some(a) = install.iter().find(|a| a.contains('\0')) {
            return Err(KeyError::BadValue(a.clone(), "install argument"));
        }

        let node_abi = platform
            .node_abi
            .map_or_else(|| "none".to_owned(), |v| v.to_string());
        Ok(vec![
            part("format", FORMAT),
            part("ecosystem", ecosystem.as_str()),
            part("root", root),
            part("manager", manager.as_str()),
            part("manager_version", manager_version),
            part("lockfiles", files_digest(lockfiles)?),
            part("manifests", files_digest(manifests)?),
            part("config", files_digest(config)?),
            part("os", &platform.os),
            part("arch", &platform.arch),
            part("libc", platform.libc.to_string()),
            part("node_abi", node_abi),
            part("install", install.join("\0")),
            part("scripts", scripts_value(scripts)?),
            part("env", env_value(env)?),
        ])
    }

    /// The key of these inputs.
    pub fn key(&self) -> Result<SnapshotKey, KeyError> {
        SnapshotKey::from_parts(&self.parts()?)
    }
}

/// The key of one snapshot: 32 bytes, written as 64 hex characters.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnapshotKey(pub [u8; 32]);

impl SnapshotKey {
    /// Hashes `parts`, which must be exactly [`PART_NAMES`] in that order (what
    /// [`KeyParts::parts`] returns, or what a record holds). A consumer recomputes the key of a
    /// record's parts with this and refuses a record whose `key` differs.
    pub fn from_parts(parts: &[(String, String)]) -> Result<Self, KeyError> {
        if parts.len() != PART_NAMES.len() {
            let at = parts.len().min(PART_NAMES.len());
            return Err(KeyError::BadParts {
                at,
                expected: PART_NAMES.get(at).copied().unwrap_or("end of parts"),
            });
        }
        let mut h = blake3::Hasher::new_derive_key(KEY_CONTEXT);
        for (at, ((name, value), expected)) in parts.iter().zip(PART_NAMES).enumerate() {
            if name != expected {
                return Err(KeyError::BadParts { at, expected });
            }
            put(&mut h, name.as_bytes());
            put(&mut h, value.as_bytes());
        }
        Ok(Self(*h.finalize().as_bytes()))
    }

    /// 32 bytes from 64 hex digits (any case).
    pub fn parse(text: &str) -> Option<Self> {
        let b = text.as_bytes();
        if b.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (o, pair) in out.iter_mut().zip(b.chunks(2)) {
            let d = |c: u8| (c as char).to_digit(16);
            *o = (d(pair[0])? * 16 + d(pair[1])?) as u8;
        }
        Some(Self(out))
    }
}

impl fmt::Display for SnapshotKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex(&self.0))
    }
}

impl fmt::Debug for SnapshotKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SnapshotKey({})", &hex(&self.0)[..12])
    }
}

/// One part that differs between two keys' parts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartDiff {
    pub name: String,
    /// `None`: the left side has no such part.
    pub left: Option<String>,
    pub right: Option<String>,
}

/// The parts that differ between two records' parts, in the order of `left` and then the parts
/// only `right` has. `asc deps key --explain` prints this. Equal parts are not listed, so equal
/// inputs give an empty list.
pub fn explain(left: &[(String, String)], right: &[(String, String)]) -> Vec<PartDiff> {
    let find = |side: &[(String, String)], name: &str| {
        side.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone())
    };
    let mut out = Vec::new();
    for (name, value) in left {
        let r = find(right, name);
        if r.as_ref() != Some(value) {
            out.push(PartDiff {
                name: name.clone(),
                left: Some(value.clone()),
                right: r,
            });
        }
    }
    for (name, value) in right {
        if find(left, name).is_none() {
            out.push(PartDiff {
                name: name.clone(),
                left: None,
                right: Some(value.clone()),
            });
        }
    }
    out
}

/// Removes the lines of an install configuration file whose key holds a credential
/// (`SECRET_KEYS`, any case), so a credential never enters a key and a rotated token does not miss.
///
/// The key of a line is the text before the first `=` (`.npmrc`, `.yarnrc`, TOML), or before the
/// first `:` followed by a blank or the end of the line (YAML), whichever comes first. The result
/// has `\n` line ends, a final one included, and no byte order mark, so a CRLF checkout gives the
/// same key as an LF one.
pub fn scrub_config(bytes: &[u8]) -> Vec<u8> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let mut out = Vec::with_capacity(bytes.len());
    for line in bytes.split(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if is_secret_line(line) {
            continue;
        }
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    // `split` yields an empty last item for text that ends in a newline; drop the line it made.
    if bytes.last() == Some(&b'\n') || bytes.is_empty() {
        out.pop();
    }
    out
}

fn is_secret_line(line: &[u8]) -> bool {
    let end = (0..line.len())
        .find(|&i| match line[i] {
            b'=' => true,
            b':' => line.get(i + 1).is_none_or(|c| *c == b' ' || *c == b'\t'),
            _ => false,
        })
        .unwrap_or(line.len());
    let key = line[..end].to_ascii_lowercase();
    SECRET_KEYS
        .iter()
        .any(|s| key.windows(s.len()).any(|w| w == s.as_bytes()))
}

/// The digest of a set of files, in hex: the BLAKE3 of the sorted `(path, digest)` pairs, each
/// path length-prefixed, so the order the files were found in does not matter.
pub fn files_digest(files: &[FileDigest]) -> Result<String, KeyError> {
    let mut sorted: Vec<&FileDigest> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    if let Some(w) = sorted.windows(2).find(|w| w[0].path == w[1].path) {
        return Err(KeyError::DuplicateFile(w[0].path.clone()));
    }
    let mut h = blake3::Hasher::new();
    for f in sorted {
        put(&mut h, f.path.as_bytes());
        h.update(&f.digest);
    }
    Ok(hex(h.finalize().as_bytes()))
}

fn scripts_value(scripts: &Scripts) -> Result<String, KeyError> {
    Ok(match scripts {
        Scripts::None => "none".to_owned(),
        Scripts::All => "all".to_owned(),
        Scripts::Allow(names) => {
            let mut sorted: Vec<&str> = names.iter().map(String::as_str).collect();
            sorted.sort_unstable();
            sorted.dedup();
            let mut h = blake3::Hasher::new();
            for name in sorted {
                check_token(name, "package name")?;
                put(&mut h, name.as_bytes());
            }
            format!("allow:{}", hex(h.finalize().as_bytes()))
        }
    })
}

/// `NAME=value` (or `NAME` for a variable that is not set), sorted by name, NUL-separated.
fn env_value(env: &[EnvEntry]) -> Result<String, KeyError> {
    let mut sorted: Vec<&EnvEntry> = env.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let mut out = Vec::with_capacity(sorted.len());
    for (i, e) in sorted.iter().enumerate() {
        check_env_name(&e.name)?;
        if sorted.get(i + 1).is_some_and(|n| n.name == e.name) {
            return Err(KeyError::DuplicateEnv(e.name.clone()));
        }
        match &e.value {
            Some(v) if v.contains('\0') => {
                return Err(KeyError::BadValue(e.name.clone(), "environment value"));
            }
            Some(v) => out.push(format!("{}={v}", e.name)),
            None => out.push(e.name.clone()),
        }
    }
    Ok(out.join("\0"))
}

/// A name of the form `[A-Za-z_][A-Za-z0-9_]*` that holds none of `TOKEN`, `SECRET`, `PASSWORD`
/// or `AUTH` in any case.
pub fn check_env_name(name: &str) -> Result<(), KeyError> {
    let mut chars = name.chars();
    let ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        return Err(KeyError::BadValue(
            name.to_owned(),
            "environment variable name",
        ));
    }
    let upper = name.to_ascii_uppercase();
    if SECRET_ENV.iter().any(|s| upper.contains(s)) {
        return Err(KeyError::SecretEnvName(name.to_owned()));
    }
    Ok(())
}

/// Non-empty, `/`-separated, with no empty, `.` or `..` component, no backslash and no NUL.
fn check_path(path: &str) -> Result<(), KeyError> {
    let bad = path.is_empty()
        || path.contains(['\\', '\0'])
        || path.split('/').any(|c| matches!(c, "" | "." | ".."));
    if bad {
        return Err(KeyError::BadPath(path.to_owned()));
    }
    Ok(())
}

/// Printable ASCII without blanks, at most 214 bytes (npm's name limit): a version or a package
/// name.
fn check_token(value: &str, what: &'static str) -> Result<(), KeyError> {
    if value.is_empty() || value.len() > 214 || !value.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(KeyError::BadValue(value.to_owned(), what));
    }
    Ok(())
}

fn check_token_lower(value: &str, what: &'static str) -> Result<(), KeyError> {
    let ok = !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if !ok {
        return Err(KeyError::BadValue(value.to_owned(), what));
    }
    Ok(())
}

fn part(name: &str, value: impl Into<String>) -> (String, String) {
    (name.to_owned(), value.into())
}

/// Length-prefixed, so no two sequences of parts hash alike.
fn put(h: &mut blake3::Hasher, bytes: &[u8]) {
    h.update(&(bytes.len() as u64).to_le_bytes());
    h.update(bytes);
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> KeyParts {
        KeyParts {
            ecosystem: Ecosystem::Npm,
            root: "apps/web".into(),
            manager: Manager::Pnpm,
            manager_version: "9.12.0".into(),
            lockfiles: vec![FileDigest::raw("pnpm-lock.yaml", b"lockfileVersion: 9.0\n")],
            manifests: vec![
                FileDigest::raw("package.json", b"{\"name\":\"web\"}\n"),
                FileDigest::raw("packages/ui/package.json", b"{\"name\":\"ui\"}\n"),
            ],
            config: vec![
                FileDigest::config(".npmrc", b"node-linker=isolated\n"),
                FileDigest::config("patches/a.patch", b"--- a\n+++ b\n"),
            ],
            platform: Platform {
                os: "linux".into(),
                arch: "x64".into(),
                libc: Libc::Glibc {
                    major: 2,
                    minor: 31,
                },
                node_abi: Some(127),
            },
            install: vec!["install".into(), "--frozen-lockfile".into()],
            scripts: Scripts::Allow(vec!["sharp".into(), "esbuild".into()]),
            env: vec![EnvEntry {
                name: "NODE_ENV".into(),
                value: Some("production".into()),
            }],
        }
    }

    fn key(p: &KeyParts) -> SnapshotKey {
        p.key().unwrap()
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    type Mutation = (&'static str, fn(&mut KeyParts));

    /// One mutation per part, in `PART_NAMES` order (the ecosystem has one variant today).
    fn mutations() -> Vec<Mutation> {
        vec![
            ("root", |p| p.root = "apps/api".into()),
            ("manager", |p| p.manager = Manager::Npm),
            ("manager_version", |p| p.manager_version = "9.12.1".into()),
            ("lockfiles", |p| {
                p.lockfiles = vec![FileDigest::raw("pnpm-lock.yaml", b"lockfileVersion: 9.1\n")];
            }),
            ("manifests", |p| {
                p.manifests[1] = FileDigest::raw("packages/ui/package.json", b"{}\n");
            }),
            ("config", |p| {
                p.config[0] = FileDigest::config(".npmrc", b"node-linker=hoisted\n");
            }),
            ("os", |p| p.platform.os = "darwin".into()),
            ("arch", |p| p.platform.arch = "arm64".into()),
            ("libc", |p| {
                p.platform.libc = Libc::Glibc {
                    major: 2,
                    minor: 35,
                };
            }),
            ("node_abi", |p| p.platform.node_abi = Some(131)),
            ("install", |p| p.install.push("--prod".into())),
            ("scripts", |p| p.scripts = Scripts::All),
            ("env", |p| p.env[0].value = Some("development".into())),
        ]
    }

    #[test]
    fn each_part_changes_the_key() {
        let a = key(&base());
        let mut seen = vec![a];
        for (name, mutate) in mutations() {
            let mut p = base();
            mutate(&mut p);
            let k = key(&p);
            assert_ne!(k, a, "{name}");
            assert!(!seen.contains(&k), "{name} collides with another change");
            seen.push(k);
            let diff = explain(&base().parts().unwrap(), &p.parts().unwrap());
            assert_eq!(diff.len(), 1, "{name}: {diff:?}");
            assert_eq!(diff[0].name, name);
        }
        assert_eq!(a, key(&base()), "the key is a function of the inputs");
        assert_eq!(base().parts().unwrap().len(), PART_NAMES.len());
    }

    #[test]
    fn variants_of_the_small_parts_change_the_key() {
        let a = key(&base());
        let change = |f: fn(&mut KeyParts)| {
            let mut p = base();
            f(&mut p);
            key(&p)
        };
        let all = [
            change(|p| p.platform.libc = Libc::Musl),
            change(|p| p.platform.libc = Libc::None),
            change(|p| p.platform.node_abi = None),
            change(|p| p.scripts = Scripts::None),
            change(|p| p.scripts = Scripts::Allow(vec!["sharp".into()])),
            change(|p| p.env.clear()),
            change(|p| p.env[0].value = None),
            change(|p| p.env[0].value = Some(String::new())),
            change(|p| p.install.clear()),
            change(|p| p.root.clear()),
            change(|p| p.manager = Manager::Bun),
        ];
        for (i, k) in all.iter().enumerate() {
            assert_ne!(*k, a, "{i}");
            assert!(!all[..i].contains(k), "{i}");
        }
    }

    #[test]
    fn parts_are_separated() {
        let (mut x, mut y) = (base(), base());
        x.install = strings(&["a", "bc"]);
        y.install = strings(&["ab", "c"]);
        assert_ne!(key(&x), key(&y));
        x.install = strings(&["a b"]);
        y.install = strings(&["a", "b"]);
        assert_ne!(key(&x), key(&y));
        x.install = strings(&["a"]);
        y.install = strings(&["a", ""]);
        assert_ne!(key(&x), key(&y));
    }

    #[test]
    fn file_order_does_not_change_the_key() {
        let a = key(&base());
        let mut p = base();
        p.manifests.reverse();
        p.config.reverse();
        assert_eq!(key(&p), a);

        let mut p = base();
        p.lockfiles.push(FileDigest::raw("yarn.lock", b"# yarn\n"));
        let one = key(&p);
        p.lockfiles.reverse();
        assert_eq!(key(&p), one);
        assert_ne!(one, a, "a second lockfile is a different install");
    }

    #[test]
    fn allow_list_order_and_repeats_do_not_change_the_key() {
        let a = key(&base());
        let mut p = base();
        p.scripts = Scripts::Allow(strings(&["esbuild", "sharp", "esbuild"]));
        assert_eq!(key(&p), a);
        p.scripts = Scripts::Allow(strings(&["esbuild", "sharp", "other"]));
        assert_ne!(key(&p), a);
    }

    #[test]
    fn env_order_does_not_change_the_key() {
        let env = |name: &str, v: &str| EnvEntry {
            name: name.into(),
            value: Some(v.into()),
        };
        let mut p = base();
        p.env = vec![env("B", "2"), env("A", "1")];
        let a = key(&p);
        p.env.reverse();
        assert_eq!(key(&p), a);
        p.env.push(env("A", "1"));
        assert_eq!(p.key(), Err(KeyError::DuplicateEnv("A".into())));
    }

    #[test]
    fn credential_lines_do_not_change_the_key() {
        let npmrc = |text: &str| {
            let mut p = base();
            p.config[0] = FileDigest::config(".npmrc", text.as_bytes());
            key(&p)
        };
        let plain = npmrc("node-linker=isolated\n");
        assert_eq!(
            npmrc("node-linker=isolated\n//registry.npmjs.org/:_authToken=abc123\n"),
            plain
        );
        assert_eq!(
            npmrc("//registry.npmjs.org/:_authToken=rotated\r\nnode-linker=isolated\r\n"),
            plain,
            "a rotated token and CRLF line ends do not miss"
        );
        for line in [
            "_auth=Zm9v",
            "//r.example/:_password=eHl6",
            "//r.example/:username=me",
            "email=me@example.com",
            "certfile=/home/me/c.pem",
            "keyfile=/home/me/k.pem",
            "cafile=/home/me/ca.pem",
            "//R.EXAMPLE/:_AUTHTOKEN=x",
        ] {
            assert_eq!(
                npmrc(&format!("{line}\nnode-linker=isolated\n")),
                plain,
                "{line}"
            );
        }
        assert_ne!(npmrc("node-linker=hoisted\n"), plain);
        assert_ne!(
            npmrc("node-linker=isolated\nhoist=false\n"),
            plain,
            "a line that is not a credential counts"
        );
    }

    #[test]
    fn scrub_config_by_example() {
        let s = |t: &str| String::from_utf8(scrub_config(t.as_bytes())).unwrap();
        assert_eq!(s(""), "");
        assert_eq!(s("a=1"), "a=1\n");
        assert_eq!(s("a=1\n"), "a=1\n");
        assert_eq!(s("a=1\n\n"), "a=1\n\n");
        assert_eq!(s("\u{feff}a=1\r\nb=2\r\n"), "a=1\nb=2\n");
        assert_eq!(s("a=1\n_authToken=x"), "a=1\n");
        assert_eq!(s("_authToken=x\n"), "");
        // YAML: the key ends at the colon that is followed by a blank.
        assert_eq!(
            s(
                "nodeLinker: node-modules\nnpmRegistries:\n  \"//r.example\":\n    npmAuthToken: abc==\n"
            ),
            "nodeLinker: node-modules\nnpmRegistries:\n  \"//r.example\":\n"
        );
        // A credential word in the value is not in the key.
        assert_eq!(
            s("registry=https://token.example/\n"),
            "registry=https://token.example/\n"
        );
        assert_eq!(s("a=b=_auth\n"), "a=b=_auth\n");
        // TOML
        assert_eq!(
            s("[install]\ntoken = \"t\"\nlinker = \"hoisted\"\n"),
            "[install]\nlinker = \"hoisted\"\n"
        );
    }

    #[test]
    fn only_settings_files_are_scrubbed() {
        let text = b"email=me@example.com\nnode-linker=isolated\n";
        let scrubbed = FileDigest::config("apps/web/.npmrc", text);
        assert_eq!(
            scrubbed.digest,
            FileDigest::raw("x", b"node-linker=isolated\n").digest
        );
        let patch = FileDigest::config("patches/p.patch", text);
        assert_eq!(patch.digest, FileDigest::raw("x", text).digest);
        let code = FileDigest::config(".pnpmfile.cjs", text);
        assert_eq!(code.digest, FileDigest::raw("x", text).digest);
    }

    #[test]
    fn credential_looking_env_names_are_refused() {
        for name in [
            "NPM_TOKEN",
            "npm_config__authToken",
            "GH_SECRET",
            "MY_password",
            "AUTHORIZATION",
            "NODE_AUTH_TOKEN",
        ] {
            assert_eq!(
                check_env_name(name),
                Err(KeyError::SecretEnvName(name.into())),
                "{name}"
            );
            let mut p = base();
            p.env[0].name = name.into();
            assert_eq!(p.key(), Err(KeyError::SecretEnvName(name.into())));
        }
        for name in ["NODE_ENV", "npm_config_registry", "CI", "_X1"] {
            assert_eq!(check_env_name(name), Ok(()), "{name}");
        }
        for name in ["", "1A", "A-B", "A=B", "A B"] {
            assert!(
                matches!(check_env_name(name), Err(KeyError::BadValue(..))),
                "{name}"
            );
        }
        let got = EnvEntry::read(&strings(&["A", "B"]), |n| {
            (n == "A").then(|| "1".to_owned())
        });
        assert_eq!(
            got,
            Ok(vec![
                EnvEntry {
                    name: "A".into(),
                    value: Some("1".into())
                },
                EnvEntry {
                    name: "B".into(),
                    value: None
                },
            ])
        );
        assert_eq!(
            EnvEntry::read(&strings(&["A_TOKEN"]), |_| None),
            Err(KeyError::SecretEnvName("A_TOKEN".into()))
        );
    }

    #[test]
    fn inputs_are_checked() {
        let bad = |f: fn(&mut KeyParts)| {
            let mut p = base();
            f(&mut p);
            p.key().unwrap_err()
        };
        for root in ["/a", "a/", "a//b", "a/./b", "a/../b", "a\\b", "."] {
            let mut p = base();
            p.root = root.into();
            assert_eq!(p.key(), Err(KeyError::BadPath(root.into())), "{root}");
        }
        assert_eq!(bad(|p| p.lockfiles.clear()), KeyError::NoLockfile);
        assert_eq!(
            bad(|p| p.lockfiles[0].path = "a.lock".into()),
            KeyError::NotLockfile("a.lock".into())
        );
        assert_eq!(
            bad(|p| {
                p.manifests.remove(0);
            }),
            KeyError::NoRootManifest
        );
        assert_eq!(
            bad(|p| p.manifests[1].path = "node_modules/x/package.json".into()),
            KeyError::NotManifest("node_modules/x/package.json".into())
        );
        assert_eq!(
            bad(|p| p.manifests[1].path = "packages/ui/package.json5".into()),
            KeyError::NotManifest("packages/ui/package.json5".into())
        );
        assert_eq!(
            bad(|p| p.manifests.push(p.manifests[0].clone())),
            KeyError::DuplicateFile("package.json".into())
        );
        assert_eq!(bad(|p| p.install = strings(&[""])), KeyError::EmptyArg);
        assert!(matches!(
            bad(|p| p.install = strings(&["a\0b"])),
            KeyError::BadValue(..)
        ));
        assert!(matches!(
            bad(|p| p.manager_version = "9 .1".into()),
            KeyError::BadValue(..)
        ));
        assert!(matches!(
            bad(|p| p.platform.os = "Linux".into()),
            KeyError::BadValue(..)
        ));
        assert!(matches!(
            bad(|p| p.config[0].path = "..\\x".into()),
            KeyError::BadPath(..)
        ));
    }

    #[test]
    fn explain_names_the_differing_parts() {
        let a = base().parts().unwrap();
        let mut p = base();
        p.manager_version = "9.13.0".into();
        p.platform.node_abi = None;
        p.env.clear();
        let b = p.parts().unwrap();
        let diff = explain(&a, &b);
        let names: Vec<&str> = diff.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["manager_version", "node_abi", "env"]);
        assert_eq!(diff[0].left.as_deref(), Some("9.12.0"));
        assert_eq!(diff[0].right.as_deref(), Some("9.13.0"));
        assert_eq!(diff[1].left.as_deref(), Some("127"));
        assert_eq!(diff[1].right.as_deref(), Some("none"));
        assert_eq!(explain(&a, &a), []);
        // A part only one side has.
        let short = &a[..a.len() - 1];
        assert_eq!(
            explain(short, &a),
            [PartDiff {
                name: "env".into(),
                left: None,
                right: Some("NODE_ENV=production".into()),
            }]
        );
        assert_eq!(explain(&a, short)[0].right, None);
    }

    #[test]
    fn from_parts_checks_names_and_recomputes() {
        let parts = base().parts().unwrap();
        assert_eq!(SnapshotKey::from_parts(&parts), Ok(key(&base())));
        let mut swapped = parts.clone();
        swapped.swap(0, 1);
        assert_eq!(
            SnapshotKey::from_parts(&swapped),
            Err(KeyError::BadParts {
                at: 0,
                expected: "format"
            })
        );
        assert_eq!(
            SnapshotKey::from_parts(&parts[..3]),
            Err(KeyError::BadParts {
                at: 3,
                expected: "manager"
            })
        );
        let mut long = parts.clone();
        long.push(("x".into(), String::new()));
        assert_eq!(
            SnapshotKey::from_parts(&long),
            Err(KeyError::BadParts {
                at: 15,
                expected: "end of parts"
            })
        );
        // A record that edits a part (clear text) no longer matches its key.
        let mut edited = parts;
        edited[3].1 = "npm".into();
        assert_ne!(SnapshotKey::from_parts(&edited), Ok(key(&base())));
    }

    #[test]
    fn hex_round_trips() {
        let k = key(&base());
        assert_eq!(k.to_string().len(), 64);
        assert_eq!(SnapshotKey::parse(&k.to_string()), Some(k));
        assert_eq!(SnapshotKey::parse(&k.to_string().to_uppercase()), Some(k));
        assert_eq!(SnapshotKey::parse("abc"), None);
        assert_eq!(SnapshotKey::parse(&"g".repeat(64)), None);
        assert_eq!(SnapshotKey::parse(&format!("{}é", "a".repeat(62))), None);
        assert!(format!("{k:?}").starts_with("SnapshotKey("));
    }

    #[test]
    fn lockfiles_are_read_without_following_links() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pnpm-lock.yaml"), b"a").unwrap();
        std::fs::write(dir.path().join("bun.lockb"), [0u8, 159, 146, 150]).unwrap();
        std::fs::write(dir.path().join("other.lock"), b"x").unwrap();
        std::fs::create_dir(dir.path().join("yarn.lock")).unwrap();
        let got = read_lockfiles(dir.path()).unwrap();
        let names: Vec<&str> = got.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(names, ["bun.lockb", "pnpm-lock.yaml"]);
        assert_eq!(got[1], FileDigest::raw("pnpm-lock.yaml", b"a"));
        assert!(
            read_lockfiles(&dir.path().join("missing"))
                .unwrap()
                .is_empty()
        );
    }

    // Golden vectors. They pin the construction: a change here changes every key, so it needs a
    // new `FORMAT` and `KEY_CONTEXT`. The same inputs must give the same bytes on every platform.

    #[test]
    fn golden_blake3_anchor() {
        // The BLAKE3 test vector for the empty input: the hash is the one the key builds on.
        assert_eq!(
            hex(blake3::hash(b"").as_bytes()),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn golden_scrub_and_file_digests() {
        let npmrc =
            b"//registry.npmjs.org/:_authToken=abc\r\nnode-linker=isolated\r\nemail=x@y\r\n";
        assert_eq!(scrub_config(npmrc), b"node-linker=isolated\n");
        assert_eq!(
            hex(&FileDigest::config(".npmrc", npmrc).digest),
            hex(blake3::hash(b"node-linker=isolated\n").as_bytes())
        );
        assert_eq!(files_digest(&[]).unwrap(), GOLDEN_EMPTY_FILES);
        let files = [
            FileDigest::raw("package.json", b"{}\n"),
            FileDigest::raw("packages/a/package.json", b"{\"name\":\"a\"}\n"),
        ];
        assert_eq!(files_digest(&files).unwrap(), GOLDEN_TWO_FILES);
        assert_eq!(
            scripts_value(&Scripts::Allow(strings(&["sharp", "esbuild"]))).unwrap(),
            GOLDEN_ALLOW
        );
    }

    #[test]
    fn golden_keys() {
        let minimal = KeyParts {
            ecosystem: Ecosystem::Npm,
            root: String::new(),
            manager: Manager::Npm,
            manager_version: "10.8.2".into(),
            lockfiles: vec![FileDigest::raw("package-lock.json", b"{}\n")],
            manifests: vec![FileDigest::raw("package.json", b"{}\n")],
            config: vec![],
            platform: Platform {
                os: "win32".into(),
                arch: "x64".into(),
                libc: Libc::None,
                node_abi: None,
            },
            install: strings(&["ci"]),
            scripts: Scripts::All,
            env: vec![],
        };
        assert_eq!(minimal.key().unwrap().to_string(), GOLDEN_MINIMAL);
        assert_eq!(base().key().unwrap().to_string(), GOLDEN_FULL);
        let text: Vec<String> = base()
            .parts()
            .unwrap()
            .iter()
            .map(|(n, v)| format!("{n}={}", v.replace('\0', "\\0")))
            .collect();
        assert_eq!(text.join("\n"), GOLDEN_PARTS);
    }

    const GOLDEN_EMPTY_FILES: &str =
        "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
    const GOLDEN_TWO_FILES: &str =
        "d26967f514beea75139dd23010893ba490231dcdbdda7eb484d9047b2faf294d";
    const GOLDEN_ALLOW: &str =
        "allow:f7a23aadf2d49afe41648aeb90cc43c177209b4646b5d984278f9f17340362e7";
    const GOLDEN_MINIMAL: &str = "93570fc188a99f4874a4579561038fbf8a56031764cc2ab74bd82bddbb99ab42";
    const GOLDEN_FULL: &str = "9b3b425b3b9175609a9d3727e793683d12cc98808d85a0e1e39a5f789995d6f4";
    const GOLDEN_PARTS: &str = "format=1
ecosystem=npm
root=apps/web
manager=pnpm
manager_version=9.12.0
lockfiles=7776c8df325a6a7561288014e845ad8c033c0cb238af7bc5993d1bd23f0ca17d
manifests=d02a3f1048dec627208e2fc64de9e224f47d61b991696261b70c5ba7136dc6f7
config=c2cadc5037493dbf3ac691403b6b2eb052d310a5cf0213632c134b2b07646009
os=linux
arch=x64
libc=glibc-2.31
node_abi=127
install=install\\0--frozen-lockfile
scripts=allow:f7a23aadf2d49afe41648aeb90cc43c177209b4646b5d984278f9f17340362e7
env=NODE_ENV=production";
}
