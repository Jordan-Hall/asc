//! Project graph model (SPEC §12A.1) and its derived-value envelopes (SPEC §12A.5).

use std::collections::BTreeMap;
use std::fmt;

use asc_core::id::{FileId, TreeId};
use asc_core::object::Kind;
use asc_core::path::RepoPath;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::canon::{CanonError, Value};
use crate::config::ResolvedConfig;

/// Nesting depth limit for manifests and lockfiles.
pub const MAX_DEPTH: usize = 128;
/// Largest manifest, always an inline object.
pub const MAX_MANIFEST_BYTES: usize = 4 << 20;
/// Largest lockfile.
pub const MAX_LOCKFILE_BYTES: usize = 64 << 20;
/// Most packages read from one lockfile.
pub const MAX_LOCKFILE_PACKAGES: usize = 200_000;
/// Most projects in one graph.
pub const MAX_PROJECTS: usize = 100_000;

/// Envelope kinds of the derived values (SPEC §12A.5).
pub const KIND_MSET: u8 = 20;
pub const KIND_PARSED: u8 = 21;
pub const KIND_PGRAPH: u8 = 22;
pub const KIND_PHASH: u8 = 23;

/// Why a model value was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelError {
    #[error("bad envelope magic or truncated header")]
    BadMagic,
    #[error("envelope kind {got} where {expected} was expected")]
    WrongKind { expected: u8, got: u8 },
    #[error("unknown version {0}")]
    UnknownVersion(u8),
    #[error("payload: {0}")]
    Payload(String),
    #[error("trailing bytes after payload")]
    Trailing,
    #[error("non-canonical encoding")]
    NonCanonical,
    #[error("invalid {0}")]
    Invalid(&'static str),
    #[error(transparent)]
    Canon(#[from] CanonError),
}

/// Ecosystem; the postcard variant order is frozen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Ecosystem {
    Rust = 0,
    Python = 1,
    Dotnet = 2,
    Node = 3,
    Declared = 4,
}

impl Ecosystem {
    pub const ALL: [Ecosystem; 5] = [
        Self::Rust,
        Self::Python,
        Self::Dotnet,
        Self::Node,
        Self::Declared,
    ];

    /// The display spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::Dotnet => "dotnet",
            Self::Node => "node",
            Self::Declared => "declared",
        }
    }

    /// Parses a display spelling or an input alias.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "rust" | "cargo" => Self::Rust,
            "python" | "uv" => Self::Python,
            "dotnet" | "csharp" | "fsharp" => Self::Dotnet,
            "node" | "bun" | "js" => Self::Node,
            "declared" => Self::Declared,
            _ => return None,
        })
    }
}

impl fmt::Display for Ecosystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ProjectKind {
    Package = 0,
    Workspace = 1,
    Solution = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum DepKind {
    Normal = 0,
    Dev = 1,
    Build = 2,
    Peer = 3,
    Optional = 4,
    Implicit = 5,
    Contains = 6,
    Member = 7,
}

/// Project identity; unique within a graph.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProjectKey {
    pub eco: Ecosystem,
    pub manifest: RepoPath,
}

/// A 32-byte configuration or dependency fingerprint (SPEC §12A.4).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for Fingerprint {
    fn from(b: [u8; 32]) -> Self {
        Self(b)
    }
}

fn write_hex(f: &mut fmt::Formatter<'_>, b: &[u8]) -> fmt::Result {
    b.iter().try_for_each(|x| write!(f, "{x:02x}"))
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(f, &self.0[..6])
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub key: ProjectKey,
    /// Crate name, PEP 503 distribution name, project file stem, package.json name or solution file name.
    pub name: Vec<u8>,
    pub kind: ProjectKind,
    /// Manifest directory; `None` is the repository root.
    pub root: Option<RepoPath>,
    /// Sorted, unique.
    pub deps: Vec<(ProjectKey, DepKind)>,
    /// Repo-relative globs the manifest references outside `root`.
    pub extra_inputs: Vec<Vec<u8>>,
    pub config_fp: Fingerprint,
    pub external_fp: Fingerprint,
    pub tags: Vec<Vec<u8>>,
    /// `Some(G)` when the project lives inside gitlink `G`.
    pub gitlink: Option<RepoPath>,
    /// Manifest unparsable, over a limit, or using an unsupported construct.
    pub opaque: bool,
}

impl Project {
    /// A project with no dependencies, inputs or fingerprints.
    pub fn new(key: ProjectKey, name: impl Into<Vec<u8>>, kind: ProjectKind) -> Self {
        let root = key.manifest.parent();
        Self {
            key,
            name: name.into(),
            kind,
            root,
            deps: Vec::new(),
            extra_inputs: Vec::new(),
            config_fp: Fingerprint::default(),
            external_fp: Fingerprint::default(),
            tags: Vec::new(),
            gitlink: None,
            opaque: false,
        }
    }

    /// The name as text.
    pub fn name_lossy(&self) -> String {
        String::from_utf8_lossy(&self.name).into_owned()
    }

    /// The root as text; `.` for the repository root.
    pub fn root_lossy(&self) -> String {
        self.root
            .as_ref()
            .map_or_else(|| ".".to_owned(), ToString::to_string)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub path: RepoPath,
    pub message: Vec<u8>,
}

/// The project graph of one manifest set; `projects` is sorted by key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectGraph {
    pub projects: Vec<Project>,
    pub config: ResolvedConfig,
    pub issues: Vec<Issue>,
}

/// Display ids, one per project in input order: `<eco>:<name>`, or
/// `<eco>:<name>@<root>` when two projects of one ecosystem share a name.
pub fn display_ids(projects: &[Project]) -> Vec<String> {
    let mut count: BTreeMap<(Ecosystem, &[u8]), usize> = BTreeMap::new();
    for p in projects {
        *count.entry((p.key.eco, p.name.as_slice())).or_default() += 1;
    }
    projects
        .iter()
        .map(|p| {
            let base = format!("{}:{}", p.key.eco, p.name_lossy());
            if count[&(p.key.eco, p.name.as_slice())] > 1 {
                format!("{base}@{}", p.root_lossy())
            } else {
                base
            }
        })
        .collect()
}

impl ProjectGraph {
    /// Display ids in `projects` order.
    pub fn display_ids(&self) -> Vec<String> {
        display_ids(&self.projects)
    }

    fn check(&self) -> Result<(), ModelError> {
        if self.projects.len() > MAX_PROJECTS {
            return Err(ModelError::Invalid("project count"));
        }
        if self.projects.windows(2).any(|w| w[0].key >= w[1].key) {
            return Err(ModelError::Invalid("project order"));
        }
        for p in &self.projects {
            if p.deps.windows(2).any(|w| w[0] >= w[1]) {
                return Err(ModelError::Invalid("dependency order"));
            }
            if p.root != p.key.manifest.parent() && p.key.eco != Ecosystem::Declared {
                return Err(ModelError::Invalid("project root"));
            }
        }
        Ok(())
    }
}

/// A set of manifests (SPEC §12A.5): every claimed path with its content id.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestSet(Vec<(RepoPath, Kind, [u8; 32])>);

impl ManifestSet {
    /// Builds a set; the entries are sorted and duplicate paths rejected.
    pub fn new(mut entries: Vec<(RepoPath, Kind, [u8; 32])>) -> Result<Self, ModelError> {
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let s = Self(entries);
        s.check()?;
        Ok(s)
    }

    pub fn entries(&self) -> &[(RepoPath, Kind, [u8; 32])] {
        &self.0
    }

    /// `blake3::derive_key("asc v1 manifest set", envelope)`.
    pub fn id(&self) -> SetId {
        SetId(blake3::derive_key(
            "asc v1 manifest set",
            &self.to_envelope(),
        ))
    }

    fn check(&self) -> Result<(), ModelError> {
        if self.0.windows(2).any(|w| w[0].0 >= w[1].0) {
            return Err(ModelError::Invalid("manifest set order"));
        }
        if self.0.iter().any(|e| matches!(e.1, Kind::Dir)) {
            return Err(ModelError::Invalid("manifest set entry kind"));
        }
        Ok(())
    }
}

/// The id of a [`ManifestSet`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SetId([u8; 32]);

impl SetId {
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for SetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(f, &self.0)
    }
}

/// One file's parsed form (SPEC §12A.5 `parsed.v1`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Parsed(pub Value);

impl<'de> Deserialize<'de> for Parsed {
    /// Decodes with the nesting bound enforced during decode, so hostile
    /// payloads cannot overflow the stack before `validate` runs.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        serde::de::DeserializeSeed::deserialize(DepthSeed(1), d).map(Parsed)
    }
}

/// Decodes a [`Value`] at a nesting depth, failing past [`MAX_DEPTH`].
/// Mirrors the derived postcard layout of `Value` (variant index, then fields).
#[derive(Clone, Copy)]
struct DepthSeed(usize);

impl<'de> serde::de::DeserializeSeed<'de> for DepthSeed {
    type Value = Value;

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        if self.0 > MAX_DEPTH {
            return Err(serde::de::Error::custom(format!(
                "nesting deeper than {MAX_DEPTH}"
            )));
        }
        d.deserialize_enum(
            "Value",
            &[
                "Null", "Bool", "Int", "Uint", "Float", "Str", "Array", "Map",
            ],
            ValueVisitor(self.0),
        )
    }
}

struct ValueVisitor(usize);

impl<'de> serde::de::Visitor<'de> for ValueVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a Value")
    }

    fn visit_enum<A: serde::de::EnumAccess<'de>>(self, data: A) -> Result<Value, A::Error> {
        use serde::de::{Error, VariantAccess};
        let (idx, v) = data.variant::<u32>()?;
        let next = DepthSeed(self.0 + 1);
        Ok(match idx {
            0 => {
                v.unit_variant()?;
                Value::Null
            }
            1 => Value::Bool(v.newtype_variant()?),
            2 => Value::Int(v.newtype_variant()?),
            3 => Value::Uint(v.newtype_variant()?),
            4 => Value::Float(v.newtype_variant()?),
            5 => Value::Str(v.newtype_variant()?),
            6 => Value::Array(v.newtype_variant_seed(SeqSeed(next))?),
            7 => Value::Map(v.newtype_variant_seed(PairsSeed(next))?),
            _ => return Err(A::Error::custom("unknown Value variant")),
        })
    }
}

/// Cap on speculative preallocation from an untrusted length prefix.
fn capped(hint: Option<usize>) -> usize {
    hint.unwrap_or(0).min(1024)
}

struct SeqSeed(DepthSeed);

impl<'de> serde::de::DeserializeSeed<'de> for SeqSeed {
    type Value = Vec<Value>;

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_seq(self)
    }
}

impl<'de> serde::de::Visitor<'de> for SeqSeed {
    type Value = Vec<Value>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a sequence of values")
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<Value>, A::Error> {
        let mut out = Vec::with_capacity(capped(seq.size_hint()));
        while let Some(v) = seq.next_element_seed(self.0)? {
            out.push(v);
        }
        Ok(out)
    }
}

struct PairsSeed(DepthSeed);

impl<'de> serde::de::DeserializeSeed<'de> for PairsSeed {
    type Value = Vec<(String, Value)>;

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_seq(self)
    }
}

impl<'de> serde::de::Visitor<'de> for PairsSeed {
    type Value = Vec<(String, Value)>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a sequence of key/value pairs")
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(
        self,
        mut seq: A,
    ) -> Result<Vec<(String, Value)>, A::Error> {
        let mut out = Vec::with_capacity(capped(seq.size_hint()));
        while let Some(pair) = seq.next_element_seed(PairSeed(self.0))? {
            out.push(pair);
        }
        Ok(out)
    }
}

struct PairSeed(DepthSeed);

impl<'de> serde::de::DeserializeSeed<'de> for PairSeed {
    type Value = (String, Value);

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_tuple(2, self)
    }
}

impl<'de> serde::de::Visitor<'de> for PairSeed {
    type Value = (String, Value);

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a key/value pair")
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(
        self,
        mut seq: A,
    ) -> Result<(String, Value), A::Error> {
        use serde::de::Error;
        let k = seq
            .next_element::<String>()?
            .ok_or_else(|| A::Error::invalid_length(0, &self))?;
        let v = seq
            .next_element_seed(self.0)?
            .ok_or_else(|| A::Error::invalid_length(1, &self))?;
        Ok((k, v))
    }
}

/// `blake3(eco || claim || FileId)`: the key of a [`Parsed`] value.
pub fn parsed_key(eco: Ecosystem, claim: &str, file: &FileId) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&[eco as u8]);
    h.update(claim.as_bytes());
    h.update(file.as_bytes());
    *h.finalize().as_bytes()
}

/// Per-project input hashes for one target (SPEC §12A.6), sorted by key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputHashes(pub Vec<(ProjectKey, [u8; 32])>);

/// `blake3(set id || root TreeId || target)`: the key of an [`InputHashes`] value.
pub fn phash_key(set: &SetId, root: &TreeId, target: &str) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(set.as_bytes());
    h.update(root.as_bytes());
    h.update(target.as_bytes());
    *h.finalize().as_bytes()
}

/// A derived value with a fixed envelope kind: `b"as"` || kind || version || postcard.
pub trait Derived: Serialize + DeserializeOwned {
    const KIND: u8;
    const VERSION: u8 = 1;

    /// Checks the invariants decoding cannot.
    fn validate(&self) -> Result<(), ModelError>;

    /// Canonical envelope bytes.
    fn to_envelope(&self) -> Vec<u8> {
        let out = vec![b'c', b'g', Self::KIND, Self::VERSION];
        postcard::to_extend(self, out).expect("in-memory encode cannot fail")
    }

    /// Strict decode: wrong kind, trailing bytes and non-canonical forms are errors.
    fn from_envelope(bytes: &[u8]) -> Result<Self, ModelError> {
        let [b'c', b'g', kind, version, payload @ ..] = bytes else {
            return Err(ModelError::BadMagic);
        };
        if *kind != Self::KIND {
            return Err(ModelError::WrongKind {
                expected: Self::KIND,
                got: *kind,
            });
        }
        if *version != Self::VERSION {
            return Err(ModelError::UnknownVersion(*version));
        }
        let (value, rest) = postcard::take_from_bytes::<Self>(payload)
            .map_err(|e| ModelError::Payload(e.to_string()))?;
        if !rest.is_empty() {
            return Err(ModelError::Trailing);
        }
        if value.to_envelope() != bytes {
            return Err(ModelError::NonCanonical);
        }
        value.validate()?;
        Ok(value)
    }
}

impl Derived for ManifestSet {
    const KIND: u8 = KIND_MSET;
    fn validate(&self) -> Result<(), ModelError> {
        self.check()
    }
}

impl Derived for Parsed {
    const KIND: u8 = KIND_PARSED;
    fn validate(&self) -> Result<(), ModelError> {
        Ok(self.0.validate()?)
    }
}

impl Derived for ProjectGraph {
    const KIND: u8 = KIND_PGRAPH;
    fn validate(&self) -> Result<(), ModelError> {
        self.check()
    }
}

impl Derived for InputHashes {
    const KIND: u8 = KIND_PHASH;
    fn validate(&self) -> Result<(), ModelError> {
        if self.0.windows(2).any(|w| w[0].0 >= w[1].0) {
            return Err(ModelError::Invalid("input hash order"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rp(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn key(eco: Ecosystem, manifest: &str) -> ProjectKey {
        ProjectKey {
            eco,
            manifest: rp(manifest),
        }
    }

    fn proj(eco: Ecosystem, manifest: &str, name: &str) -> Project {
        Project::new(key(eco, manifest), name, ProjectKind::Package)
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn ecosystem_names_and_aliases() {
        for e in Ecosystem::ALL {
            assert_eq!(Ecosystem::parse(e.as_str()), Some(e));
        }
        let cases = [
            ("cargo", Ecosystem::Rust),
            ("uv", Ecosystem::Python),
            ("csharp", Ecosystem::Dotnet),
            ("fsharp", Ecosystem::Dotnet),
            ("bun", Ecosystem::Node),
            ("js", Ecosystem::Node),
        ];
        for (alias, e) in cases {
            assert_eq!(Ecosystem::parse(alias), Some(e));
        }
        assert_eq!(Ecosystem::parse("go"), None);
    }

    #[test]
    fn display_ids_suffix_only_on_collision() {
        let ps = vec![
            proj(Ecosystem::Rust, "a/Cargo.toml", "core"),
            proj(Ecosystem::Rust, "b/Cargo.toml", "core"),
            proj(Ecosystem::Node, "pkg/package.json", "core"),
            proj(Ecosystem::Rust, "Cargo.toml", "solo"),
            proj(Ecosystem::Node, "x/package.json", "@asc/ui"),
        ];
        assert_eq!(
            display_ids(&ps),
            [
                "rust:core@a",
                "rust:core@b",
                "node:core",
                "rust:solo",
                "node:@asc/ui"
            ]
        );
        let root = vec![
            proj(Ecosystem::Rust, "Cargo.toml", "dup"),
            proj(Ecosystem::Rust, "z/Cargo.toml", "dup"),
        ];
        assert_eq!(display_ids(&root), ["rust:dup@.", "rust:dup@z"]);
    }

    #[test]
    fn golden_ecosystem_order() {
        assert_eq!(postcard::to_allocvec(&Ecosystem::Declared).unwrap(), [4]);
        assert_eq!(postcard::to_allocvec(&DepKind::Member).unwrap(), [7]);
        assert_eq!(postcard::to_allocvec(&ProjectKind::Solution).unwrap(), [2]);
    }

    #[test]
    fn golden_mset() {
        let set = ManifestSet::new(vec![
            (rp("package.json"), Kind::File, [1; 32]),
            (rp("Cargo.toml"), Kind::File, [2; 32]),
        ])
        .unwrap();
        let env = set.to_envelope();
        assert_eq!(env[..4], [b'c', b'g', 20, 1]);
        assert_eq!(
            hex(&env),
            "63671401020a436172676f2e746f6d6c0002020202020202020202020202020202020202020202020202020202020202020c7061636b6167652e6a736f6e000101010101010101010101010101010101010101010101010101010101010101"
        );
        assert_eq!(ManifestSet::from_envelope(&env).unwrap(), set);
        assert_eq!(
            hex(set.id().as_bytes()),
            "7c0963a5c850398dd467b1dfb9752e96703d4294d65bbb5674e399ec438516af"
        );
    }

    #[test]
    fn golden_parsed() {
        let p = Parsed(Value::map([("name".into(), Value::Str("x".into()))]).unwrap());
        let env = p.to_envelope();
        assert_eq!(env[..4], [b'c', b'g', 21, 1]);
        assert_eq!(hex(&env), "636715010701046e616d65050178");
        assert_eq!(Parsed::from_envelope(&env).unwrap(), p);
    }

    fn sample_graph() -> ProjectGraph {
        let mut a = proj(Ecosystem::Rust, "crates/a/Cargo.toml", "a");
        let b = proj(Ecosystem::Node, "pkg/package.json", "b");
        a.deps = vec![(b.key.clone(), DepKind::Dev)];
        a.tags = vec![b"t".to_vec()];
        a.config_fp = Fingerprint::from([7; 32]);
        ProjectGraph {
            projects: vec![a, b],
            config: ResolvedConfig::default(),
            issues: vec![Issue {
                path: rp("x/Cargo.toml"),
                message: b"bad".to_vec(),
            }],
        }
    }

    #[test]
    fn golden_pgraph() {
        let g = sample_graph();
        let env = g.to_envelope();
        assert_eq!(env[..4], [b'c', b'g', 22, 1]);
        assert_eq!(
            hex(&env),
            "636716010200136372617465732f612f436172676f2e746f6d6c01610001086372617465732f61010310706b672f7061636b6167652e6a736f6e01000707070707070707070707070707070707070707070707070707070707070707000000000000000000000000000000000000000000000000000000000000000001017400000310706b672f7061636b6167652e6a736f6e0162000103706b670000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000100000000010c782f436172676f2e746f6d6c03626164"
        );
        assert_eq!(ProjectGraph::from_envelope(&env).unwrap(), g);
    }

    #[test]
    fn golden_phash() {
        let h = InputHashes(vec![(key(Ecosystem::Rust, "a/Cargo.toml"), [9; 32])]);
        let env = h.to_envelope();
        assert_eq!(env[..4], [b'c', b'g', 23, 1]);
        assert_eq!(
            hex(&env),
            "6367170101000c612f436172676f2e746f6d6c0909090909090909090909090909090909090909090909090909090909090909"
        );
        assert_eq!(InputHashes::from_envelope(&env).unwrap(), h);
    }

    #[test]
    fn decode_is_strict() {
        let env = sample_graph().to_envelope();
        assert_eq!(
            ProjectGraph::from_envelope(&env[..3]),
            Err(ModelError::BadMagic)
        );
        assert_eq!(
            Parsed::from_envelope(&env),
            Err(ModelError::WrongKind {
                expected: 21,
                got: 22
            })
        );
        let mut v = env.clone();
        v[3] = 2;
        assert_eq!(
            ProjectGraph::from_envelope(&v),
            Err(ModelError::UnknownVersion(2))
        );
        let mut v = env.clone();
        v.push(0);
        assert_eq!(ProjectGraph::from_envelope(&v), Err(ModelError::Trailing));
    }

    #[test]
    fn decode_rejects_unsorted() {
        let mut g = sample_graph();
        g.projects.reverse();
        assert_eq!(
            ProjectGraph::from_envelope(&g.to_envelope()),
            Err(ModelError::Invalid("project order"))
        );
        let mut g = sample_graph();
        let k = g.projects[1].key.clone();
        g.projects[0].deps = vec![(k.clone(), DepKind::Dev), (k, DepKind::Normal)];
        assert_eq!(
            ProjectGraph::from_envelope(&g.to_envelope()),
            Err(ModelError::Invalid("dependency order"))
        );
        let raw = vec![
            (rp("b"), Kind::File, [0; 32]),
            (rp("a"), Kind::File, [0; 32]),
        ];
        assert!(ManifestSet::new(raw.clone()).is_ok());
        assert_eq!(
            ManifestSet::from_envelope(&ManifestSet(raw).to_envelope()),
            Err(ModelError::Invalid("manifest set order"))
        );
    }

    /// Envelope of `depth` levels: `depth - 1` single-element arrays around `Null`.
    fn nested_parsed(depth: usize) -> Vec<u8> {
        let mut b = vec![b'c', b'g', KIND_PARSED, 1];
        for _ in 1..depth {
            b.extend_from_slice(&[6, 1]);
        }
        b.push(0);
        b
    }

    #[test]
    fn decode_bounds_nesting_during_decode() {
        assert!(Parsed::from_envelope(&nested_parsed(MAX_DEPTH)).is_ok());
        for depth in [MAX_DEPTH + 1, 100_000, 5_000_000] {
            assert!(
                matches!(
                    Parsed::from_envelope(&nested_parsed(depth)),
                    Err(ModelError::Payload(_))
                ),
                "depth {depth}"
            );
        }
        // Maps count too.
        let mut b = vec![b'c', b'g', KIND_PARSED, 1];
        for _ in 0..200 {
            b.extend_from_slice(&[7, 1, 1, b'k']);
        }
        b.push(0);
        assert!(matches!(
            Parsed::from_envelope(&b),
            Err(ModelError::Payload(_))
        ));
    }

    #[test]
    fn parsed_and_phash_keys() {
        let f = FileId::of(b"x");
        assert_ne!(
            parsed_key(Ecosystem::Rust, "Cargo.toml", &f),
            parsed_key(Ecosystem::Node, "Cargo.toml", &f)
        );
        let s = ManifestSet::default().id();
        let t = TreeId::of(b"t");
        assert_ne!(phash_key(&s, &t, "build"), phash_key(&s, &t, "test"));
    }
}
