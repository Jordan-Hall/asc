//! Stored object model (SPEC §2.4-§2.7) and `SortedMap`.

use std::fmt;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::codec::{CodecError, EnvelopeKind, Enveloped};
use crate::id::{ChangesetId, ChunkId, FileId, GitOid, TagId, TreeId};
use crate::path::{Name, RepoPath};
use crate::sig::Signature;

/// Why an object failed its invariants.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ObjectError {
    #[error("map keys are not strictly ascending")]
    UnsortedMap,
    #[error("tree entries are not strictly ascending by name")]
    UnsortedEntries,
    #[error("non-root tree is empty")]
    EmptyTree,
    #[error("gitlink entry has a non-zero id")]
    GitlinkId,
    #[error("change write kind is not File, Exec or Symlink")]
    WriteKind,
    #[error("changeset parents contain a duplicate")]
    DuplicateParent,
    #[error("chunk list has no chunks")]
    EmptyChunks,
    #[error("chunk length is zero or above the 4 MiB maximum")]
    ChunkLen,
    #[error("chunk lengths do not sum to size")]
    ChunkSum,
    #[error("chunk list size is not above the 4 MiB threshold")]
    ChunkSize,
}

// Name and RepoPath travel as plain byte strings and are validated on decode.

impl Serialize for Name {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(self.as_bytes())
    }
}

impl<'de> Deserialize<'de> for Name {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Name::new(Vec::<u8>::deserialize(d)?).map_err(D::Error::custom)
    }
}

impl Serialize for RepoPath {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(self.as_bytes())
    }
}

impl<'de> Deserialize<'de> for RepoPath {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        RepoPath::new(Vec::<u8>::deserialize(d)?).map_err(D::Error::custom)
    }
}

/// A map stored as `Vec<(K, V)>`, strictly ascending by key.
#[derive(Clone, PartialEq, Eq)]
pub struct SortedMap<K, V>(Vec<(K, V)>);

impl<K: Ord, V> SortedMap<K, V> {
    /// Sorts `items` by key; duplicate keys are an error.
    pub fn new(mut items: Vec<(K, V)>) -> Result<Self, ObjectError> {
        items.sort_by(|a, b| a.0.cmp(&b.0));
        Self::from_sorted(items)
    }

    fn from_sorted(items: Vec<(K, V)>) -> Result<Self, ObjectError> {
        if items.windows(2).all(|w| w[0].0 < w[1].0) {
            Ok(Self(items))
        } else {
            Err(ObjectError::UnsortedMap)
        }
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        self.0
            .binary_search_by(|e| e.0.cmp(key))
            .ok()
            .map(|i| &self.0[i].1)
    }
}

impl<K, V> SortedMap<K, V> {
    pub fn as_slice(&self) -> &[(K, V)] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<K, V> Default for SortedMap<K, V> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for SortedMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.0.iter().map(|(k, v)| (k, v)))
            .finish()
    }
}

impl<K: Serialize, V: Serialize> Serialize for SortedMap<K, V> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de, K: Deserialize<'de> + Ord, V: Deserialize<'de>> Deserialize<'de> for SortedMap<K, V> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::from_sorted(Vec::deserialize(d)?).map_err(D::Error::custom)
    }
}

/// Entry kind; the postcard variant index is the discriminant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    File = 0,
    Exec = 1,
    Symlink = 2,
    Dir = 3,
    Gitlink = 4,
}

/// One tree entry. `id` is a `FileId` or `TreeId`, or all zero for a gitlink.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub name: Name,
    pub kind: Kind,
    pub id: [u8; 32],
    pub size: u64,
    pub git: GitOid,
}

/// A directory: entries strictly ascending by name. The field is private so
/// every instance satisfies the invariants.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawTree")]
pub struct Tree {
    entries: Vec<Entry>,
}

#[derive(Deserialize)]
struct RawTree {
    entries: Vec<Entry>,
}

impl TryFrom<RawTree> for Tree {
    type Error = ObjectError;

    /// The wire form cannot say whether a tree is the root, so emptiness is not checked.
    fn try_from(raw: RawTree) -> Result<Self, ObjectError> {
        Tree::check(&raw.entries)?;
        Ok(Tree {
            entries: raw.entries,
        })
    }
}

impl Tree {
    /// Checks sort order, uniqueness, non-emptiness (unless `is_root`),
    /// and gitlink ids.
    pub fn new(entries: Vec<Entry>, is_root: bool) -> Result<Self, ObjectError> {
        if entries.is_empty() && !is_root {
            return Err(ObjectError::EmptyTree);
        }
        Self::check(&entries)?;
        Ok(Self { entries })
    }

    fn check(entries: &[Entry]) -> Result<(), ObjectError> {
        if !entries.windows(2).all(|w| w[0].name < w[1].name) {
            return Err(ObjectError::UnsortedEntries);
        }
        for e in entries {
            if e.kind == Kind::Gitlink && e.id != [0; 32] {
                return Err(ObjectError::GitlinkId);
            }
        }
        Ok(())
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn get(&self, name: &Name) -> Option<&Entry> {
        self.entries
            .binary_search_by(|e| e.name.cmp(name))
            .ok()
            .map(|i| &self.entries[i])
    }

    pub fn id(&self) -> TreeId {
        TreeId::of(&self.to_envelope())
    }
}

impl Enveloped for Tree {
    const KIND: EnvelopeKind = EnvelopeKind::Tree;
}

/// A leaf change relative to `parents[0]` (or the empty tree).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawChange")]
pub enum Change {
    Write {
        /// File, Exec or Symlink.
        kind: Kind,
        file: FileId,
        size: u64,
        git: GitOid,
        copy_from: Option<RepoPath>,
    },
    Gitlink {
        commit: GitOid,
    },
    Delete,
}

/// Wire mirror of `Change`; variant order must match.
#[derive(Deserialize)]
enum RawChange {
    Write {
        kind: Kind,
        file: FileId,
        size: u64,
        git: GitOid,
        copy_from: Option<RepoPath>,
    },
    Gitlink {
        commit: GitOid,
    },
    Delete,
}

impl TryFrom<RawChange> for Change {
    type Error = ObjectError;

    fn try_from(raw: RawChange) -> Result<Self, ObjectError> {
        Ok(match raw {
            RawChange::Write {
                kind,
                file,
                size,
                git,
                copy_from,
            } => Change::write(kind, file, size, git, copy_from)?,
            RawChange::Gitlink { commit } => Change::Gitlink { commit },
            RawChange::Delete => Change::Delete,
        })
    }
}

impl Change {
    /// Builds a `Write`: kind must be File, Exec or Symlink.
    pub fn write(
        kind: Kind,
        file: FileId,
        size: u64,
        git: GitOid,
        copy_from: Option<RepoPath>,
    ) -> Result<Self, ObjectError> {
        if !matches!(kind, Kind::File | Kind::Exec | Kind::Symlink) {
            return Err(ObjectError::WriteKind);
        }
        Ok(Change::Write {
            kind,
            file,
            size,
            git,
            copy_from,
        })
    }
}

/// Ancestor/descendant paths in `changes` are deliberately allowed (SPEC §2.6
/// rule 3); full apply validation happens at ingest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawChangeset")]
pub struct Changeset {
    pub parents: Vec<ChangesetId>,
    pub root: TreeId,
    pub author: Signature,
    pub committer: Signature,
    pub message: Vec<u8>,
    pub extra: SortedMap<Vec<u8>, Vec<u8>>,
    pub changes: SortedMap<RepoPath, Change>,
}

#[derive(Deserialize)]
struct RawChangeset {
    parents: Vec<ChangesetId>,
    root: TreeId,
    author: Signature,
    committer: Signature,
    message: Vec<u8>,
    extra: SortedMap<Vec<u8>, Vec<u8>>,
    changes: SortedMap<RepoPath, Change>,
}

impl TryFrom<RawChangeset> for Changeset {
    type Error = ObjectError;

    fn try_from(r: RawChangeset) -> Result<Self, ObjectError> {
        // `changes` order is enforced by `SortedMap`, each `Change` by its own decode.
        let mut seen: Vec<&ChangesetId> = r.parents.iter().collect();
        seen.sort();
        if seen.windows(2).any(|w| w[0] == w[1]) {
            return Err(ObjectError::DuplicateParent);
        }
        Ok(Changeset {
            parents: r.parents,
            root: r.root,
            author: r.author,
            committer: r.committer,
            message: r.message,
            extra: r.extra,
            changes: r.changes,
        })
    }
}

impl Changeset {
    pub fn id(&self) -> ChangesetId {
        ChangesetId::of(&self.to_envelope())
    }
}

impl Enveloped for Changeset {
    const KIND: EnvelopeKind = EnvelopeKind::Changeset;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagRecord {
    pub target: ChangesetId,
    pub name: Vec<u8>,
    pub tagger: Option<Signature>,
    pub message: Vec<u8>,
    pub extra: SortedMap<Vec<u8>, Vec<u8>>,
}

impl TagRecord {
    pub fn id(&self) -> TagId {
        TagId::of(&self.to_envelope())
    }
}

impl Enveloped for TagRecord {
    const KIND: EnvelopeKind = EnvelopeKind::TagRecord;
}

/// Files above this size are chunked (SPEC §2.5).
const CHUNK_THRESHOLD: u64 = 4_194_304;
/// FastCDC maximum chunk length.
const MAX_CHUNK_LEN: u32 = 4_194_304;

/// Chunks of a file larger than 4 MiB; each `u32` is a chunk length and they
/// sum to `size`. Not identity-bearing: stored under the file's `FileId`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawChunkList")]
pub struct ChunkList {
    pub size: u64,
    pub chunks: Vec<(ChunkId, u32)>,
}

#[derive(Deserialize)]
struct RawChunkList {
    size: u64,
    chunks: Vec<(ChunkId, u32)>,
}

impl TryFrom<RawChunkList> for ChunkList {
    type Error = ObjectError;

    fn try_from(r: RawChunkList) -> Result<Self, ObjectError> {
        if r.chunks.is_empty() {
            return Err(ObjectError::EmptyChunks);
        }
        if r.chunks.iter().any(|&(_, n)| n == 0 || n > MAX_CHUNK_LEN) {
            return Err(ObjectError::ChunkLen);
        }
        let sum = r
            .chunks
            .iter()
            .try_fold(0u64, |acc, &(_, n)| acc.checked_add(u64::from(n)));
        if sum != Some(r.size) {
            return Err(ObjectError::ChunkSum);
        }
        if r.size <= CHUNK_THRESHOLD {
            return Err(ObjectError::ChunkSize);
        }
        Ok(ChunkList {
            size: r.size,
            chunks: r.chunks,
        })
    }
}

impl Enveloped for ChunkList {
    const KIND: EnvelopeKind = EnvelopeKind::ChunkList;
}

/// Payload of the `asc` commit header (SPEC §2.2a, §2.9): version byte `0x01`
/// then postcard. Not an envelope.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AscGitExtra {
    /// `(path, copy_from)`, sorted by path.
    pub copies: SortedMap<RepoPath, RepoPath>,
    pub snapshot: bool,
}

impl AscGitExtra {
    pub const VERSION: u8 = 1;

    pub fn encode(&self) -> Vec<u8> {
        crate::codec::encode_versioned(Self::VERSION, self)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        crate::codec::decode_versioned(Self::VERSION, bytes)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::codec::{decode_envelope, encode_envelope};

    fn name(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    fn path(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn sig(t: i64) -> Signature {
        Signature::new(b"Ann".to_vec(), b"ann@example.com".to_vec(), t, -330).unwrap()
    }

    fn entry(n: &str, kind: Kind) -> Entry {
        let id = if kind == Kind::Gitlink {
            [0; 32]
        } else {
            [7; 32]
        };
        Entry {
            name: name(n),
            kind,
            id,
            size: 5,
            git: GitOid::from_bytes([9; 20]),
        }
    }

    // ---- golden samples ----

    fn samples() -> Vec<(&'static str, Vec<u8>)> {
        let tree = Tree::new(
            vec![
                entry("big.bin", Kind::File),
                entry("link", Kind::Symlink),
                entry("run.sh", Kind::Exec),
                entry("sub", Kind::Dir),
                entry("vendor", Kind::Gitlink),
            ],
            true,
        )
        .unwrap();
        let changeset = Changeset {
            parents: vec![
                ChangesetId::from_bytes([1; 32]),
                ChangesetId::from_bytes([2; 32]),
            ],
            root: tree.id(),
            author: sig(1_700_000_000),
            committer: sig(-1),
            message: b"subject\n\nbody\n".to_vec(),
            extra: SortedMap::new(vec![
                (b"asc.snapshot".to_vec(), vec![]),
                (b"git.gpgsig".to_vec(), b"sig".to_vec()),
            ])
            .unwrap(),
            changes: SortedMap::new(vec![
                (
                    path("a/b"),
                    Change::Write {
                        kind: Kind::File,
                        file: FileId::from_bytes([4; 32]),
                        size: 300,
                        git: GitOid::from_bytes([5; 20]),
                        copy_from: Some(path("a/c")),
                    },
                ),
                (
                    path("g"),
                    Change::Gitlink {
                        commit: GitOid::from_bytes([8; 20]),
                    },
                ),
                (path("z"), Change::Delete),
            ])
            .unwrap(),
        };
        let tag = TagRecord {
            target: changeset.id(),
            name: b"v1.0".to_vec(),
            tagger: Some(sig(42)),
            message: b"release".to_vec(),
            extra: SortedMap::new(vec![(b"git.tag".to_vec(), b"raw".to_vec())]).unwrap(),
        };
        let chunks = ChunkList {
            size: 6_000_000,
            chunks: vec![
                (ChunkId::from_bytes([1; 32]), 4_194_304),
                (ChunkId::from_bytes([2; 32]), 1_805_696),
            ],
        };
        let extra = AscGitExtra {
            copies: SortedMap::new(vec![(path("x"), path("y"))]).unwrap(),
            snapshot: true,
        };
        vec![
            ("tree", tree.to_envelope()),
            ("changeset", changeset.to_envelope()),
            ("tag", tag.to_envelope()),
            ("chunklist", chunks.to_envelope()),
            ("ascgitextra", extra.encode()),
        ]
    }

    #[test]
    fn golden_envelopes() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/envelopes");
        for (name, bytes) in samples() {
            let hex = data_encoding::HEXLOWER.encode(&bytes);
            let file = format!("{dir}/{name}.hex");
            if std::env::var_os("ASC_BLESS").is_some() {
                std::fs::write(&file, format!("{hex}\n")).unwrap();
            }
            let want = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{file}: {e}"));
            assert_eq!(hex, want.trim(), "{name}");
        }
    }

    #[test]
    fn golden_decodes_back() {
        let s = samples();
        let get = |n: &str| s.iter().find(|(k, _)| *k == n).unwrap().1.clone();
        assert!(decode_envelope::<Tree>(&get("tree")).is_ok());
        assert!(decode_envelope::<Changeset>(&get("changeset")).is_ok());
        assert!(decode_envelope::<TagRecord>(&get("tag")).is_ok());
        assert!(decode_envelope::<ChunkList>(&get("chunklist")).is_ok());
        assert!(AscGitExtra::decode(&get("ascgitextra")).is_ok());
    }

    #[test]
    fn empty_tree_id_is_frozen() {
        let t = Tree::new(vec![], true).unwrap();
        assert_eq!(t.to_envelope(), [b'a', b's', 1, 1, 0]);
        let want = include_str!("../tests/golden/envelopes/empty_tree_id.txt");
        assert_eq!(t.id().to_string(), want.trim());
    }

    // ---- rejection ----

    #[test]
    fn rejects_malformed_envelopes() {
        let good = Tree::new(vec![entry("a", Kind::File)], true)
            .unwrap()
            .to_envelope();
        let mut bad_magic = good.clone();
        bad_magic[0] = b'x';
        assert_eq!(
            decode_envelope::<Tree>(&bad_magic),
            Err(CodecError::BadMagic)
        );
        assert_eq!(decode_envelope::<Tree>(b"asc"), Err(CodecError::BadMagic));
        let mut kind = good.clone();
        kind[2] = 5;
        assert_eq!(
            decode_envelope::<Tree>(&kind),
            Err(CodecError::UnknownKind(5))
        );
        kind[2] = 4;
        assert!(matches!(
            decode_envelope::<Tree>(&kind),
            Err(CodecError::WrongKind { .. })
        ));
        let mut ver = good.clone();
        ver[3] = 2;
        assert!(matches!(
            decode_envelope::<Tree>(&ver),
            Err(CodecError::UnknownVersion { version: 2, .. })
        ));
        let mut trailing = good.clone();
        trailing.push(0);
        assert_eq!(
            decode_envelope::<Tree>(&trailing),
            Err(CodecError::Trailing)
        );
        assert!(matches!(
            decode_envelope::<Tree>(&good[..good.len() - 1]),
            Err(CodecError::Payload(_))
        ));
    }

    #[test]
    fn rejects_non_canonical_varint() {
        // Tree with zero entries encoded as a two-byte varint length.
        assert!(decode_envelope::<Tree>(&[b'a', b's', 1, 1, 0x80, 0x00]).is_err());
    }

    #[test]
    fn rejects_unsorted_or_duplicate_map_keys() {
        assert_eq!(
            SortedMap::new(vec![(1u8, ()), (1, ())]).unwrap_err(),
            ObjectError::UnsortedMap
        );
        assert_eq!(
            SortedMap::new(vec![(2u8, ()), (1, ())]).unwrap().as_slice()[0].0,
            1
        );
        let enc = |v: Vec<(u8, u8)>| encode_envelope(EnvelopeKind::ChunkList, 1, &v);
        for v in [vec![(2u8, 0u8), (1, 0)], vec![(1, 0), (1, 1)]] {
            let bytes = enc(v);
            let r: Result<SortedMap<u8, u8>, _> = postcard::from_bytes(&bytes[4..]);
            assert!(r.is_err());
        }
    }

    fn bare_changeset(parents: Vec<ChangesetId>, changes: Vec<(RepoPath, Change)>) -> Changeset {
        Changeset {
            parents,
            root: Tree::new(vec![], true).unwrap().id(),
            author: sig(1),
            committer: sig(1),
            message: vec![],
            extra: SortedMap::default(),
            changes: SortedMap::new(changes).unwrap(),
        }
    }

    fn write(kind: Kind) -> Change {
        Change::Write {
            kind,
            file: FileId::from_bytes([4; 32]),
            size: 1,
            git: GitOid::from_bytes([5; 20]),
            copy_from: None,
        }
    }

    #[test]
    fn rejects_bad_change_write() {
        let cs = |c: Change| bare_changeset(vec![], vec![(path("a"), c)]).to_envelope();
        for bad in [write(Kind::Dir), write(Kind::Gitlink)] {
            assert!(decode_envelope::<Changeset>(&cs(bad)).is_err());
        }
        for good in [write(Kind::File), write(Kind::Exec), write(Kind::Symlink)] {
            assert!(decode_envelope::<Changeset>(&cs(good)).is_ok());
        }
        let f = FileId::from_bytes([4; 32]);
        let g = GitOid::from_bytes([5; 20]);
        assert_eq!(
            Change::write(Kind::Dir, f, 1, g, None).unwrap_err(),
            ObjectError::WriteKind
        );
        assert!(Change::write(Kind::Symlink, f, 1, g, None).is_ok());
    }

    #[test]
    fn rejects_duplicate_parents_but_allows_ancestor_paths() {
        let p = ChangesetId::from_bytes([1; 32]);
        let changes = vec![
            (path("a"), Change::Delete),
            (path("a/b"), write(Kind::File)),
        ];
        assert!(
            decode_envelope::<Changeset>(
                &bare_changeset(vec![p, p], changes.clone()).to_envelope()
            )
            .is_err()
        );
        assert!(
            decode_envelope::<Changeset>(&bare_changeset(vec![p], changes).to_envelope()).is_ok()
        );
    }

    #[test]
    fn rejects_bad_chunk_lists() {
        let id = ChunkId::from_bytes([1; 32]);
        let max = 4_194_304u32;
        let cl = |size: u64, chunks: Vec<(ChunkId, u32)>| ChunkList { size, chunks }.to_envelope();
        let bad = [
            cl(5_000_000, vec![]),
            cl(u64::from(max) + 1, vec![(id, max), (id, 0), (id, 1)]),
            cl(u64::from(max) + 1, vec![(id, max + 1)]),
            cl(u64::from(max) + 2, vec![(id, max), (id, 1)]),
            cl(u64::from(max), vec![(id, max)]),
            cl(100, vec![(id, 100)]),
            cl(u64::MAX, vec![(id, max); 2]),
        ];
        for b in &bad {
            assert!(decode_envelope::<ChunkList>(b).is_err());
        }
        assert!(
            decode_envelope::<ChunkList>(&cl(u64::from(max) + 1, vec![(id, max), (id, 1)])).is_ok()
        );
    }

    #[test]
    fn tree_new_checks_invariants() {
        let a = entry("a", Kind::File);
        let b = entry("b", Kind::File);
        assert_eq!(
            Tree::new(vec![b.clone(), a.clone()], false).unwrap_err(),
            ObjectError::UnsortedEntries
        );
        assert_eq!(
            Tree::new(vec![a.clone(), a.clone()], false).unwrap_err(),
            ObjectError::UnsortedEntries
        );
        assert_eq!(
            Tree::new(vec![], false).unwrap_err(),
            ObjectError::EmptyTree
        );
        assert!(Tree::new(vec![], true).is_ok());
        let mut g = entry("g", Kind::Gitlink);
        g.id = [1; 32];
        assert_eq!(
            Tree::new(vec![g], true).unwrap_err(),
            ObjectError::GitlinkId
        );
    }

    #[test]
    fn decode_enforces_tree_invariants() {
        let bad = [entry("b", Kind::File), entry("a", Kind::File)];
        let bytes = encode_envelope(EnvelopeKind::Tree, 1, &bad.to_vec());
        assert!(decode_envelope::<Tree>(&bytes).is_err());
    }

    #[test]
    fn decode_rejects_invalid_names() {
        let mut e = entry("a", Kind::File);
        let bytes = encode_envelope(EnvelopeKind::Tree, 1, &vec![e.clone()]);
        // Patch the one-byte name 'a' into '/'.
        let mut bad = bytes.clone();
        let pos = bad.iter().position(|&b| b == b'a').unwrap();
        bad[pos] = b'/';
        assert!(decode_envelope::<Tree>(&bad).is_err());
        e.name = name("ok");
        assert!(decode_envelope::<Tree>(&Tree::new(vec![e], true).unwrap().to_envelope()).is_ok());
    }

    #[test]
    fn ids_differ_per_kind() {
        let t = Tree::new(vec![], true).unwrap();
        assert_eq!(t.id(), TreeId::of(&t.to_envelope()));
    }

    // ---- property tests ----

    fn arb_name() -> impl Strategy<Value = Name> {
        prop::collection::vec(any::<u8>(), 1..8)
            .prop_filter_map("invalid name", |b| Name::new(b).ok())
    }

    fn arb_path() -> impl Strategy<Value = RepoPath> {
        prop::collection::vec(arb_name(), 1..4).prop_map(|ns| {
            let parts: Vec<&[u8]> = ns.iter().map(|n| n.as_bytes()).collect();
            RepoPath::new(parts.join(&b'/')).unwrap()
        })
    }

    fn arb_kind() -> impl Strategy<Value = Kind> {
        prop_oneof![
            Just(Kind::File),
            Just(Kind::Exec),
            Just(Kind::Symlink),
            Just(Kind::Dir),
            Just(Kind::Gitlink)
        ]
    }

    fn arb_entry() -> impl Strategy<Value = Entry> {
        (
            arb_name(),
            arb_kind(),
            any::<[u8; 32]>(),
            any::<u64>(),
            any::<[u8; 20]>(),
        )
            .prop_map(|(name, kind, id, size, git)| {
                let id = if kind == Kind::Gitlink { [0; 32] } else { id };
                Entry {
                    name,
                    kind,
                    id,
                    size,
                    git: GitOid::from_bytes(git),
                }
            })
    }

    fn arb_tree() -> impl Strategy<Value = Tree> {
        prop::collection::btree_map(arb_name(), arb_entry(), 0..6).prop_map(|m| {
            let entries = m
                .into_iter()
                .map(|(n, mut e)| {
                    e.name = n;
                    e
                })
                .collect();
            Tree::new(entries, true).unwrap()
        })
    }

    fn arb_sig() -> impl Strategy<Value = Signature> {
        let text = || {
            prop::collection::vec(
                any::<u8>().prop_filter("ident byte", |c| !matches!(c, b'\n' | b'<' | b'>' | 0)),
                0..12,
            )
        };
        (text(), text(), any::<i64>(), any::<i16>()).prop_map(|(name, email, time, tz_min)| {
            Signature::new(name, email, time, tz_min).unwrap()
        })
    }

    fn arb_extra() -> impl Strategy<Value = SortedMap<Vec<u8>, Vec<u8>>> {
        prop::collection::btree_map(any::<Vec<u8>>(), any::<Vec<u8>>(), 0..4)
            .prop_map(|m| SortedMap::new(m.into_iter().collect()).unwrap())
    }

    fn arb_change() -> impl Strategy<Value = Change> {
        prop_oneof![
            (
                prop_oneof![Just(Kind::File), Just(Kind::Exec), Just(Kind::Symlink)],
                any::<[u8; 32]>(),
                any::<u64>(),
                any::<[u8; 20]>(),
                prop::option::of(arb_path()),
            )
                .prop_map(|(kind, file, size, git, copy_from)| Change::Write {
                    kind,
                    file: FileId::from_bytes(file),
                    size,
                    git: GitOid::from_bytes(git),
                    copy_from,
                }),
            any::<[u8; 20]>().prop_map(|c| Change::Gitlink {
                commit: GitOid::from_bytes(c)
            }),
            Just(Change::Delete),
        ]
    }

    fn arb_changeset() -> impl Strategy<Value = Changeset> {
        (
            prop::collection::btree_set(any::<[u8; 32]>(), 0..3),
            any::<[u8; 32]>(),
            arb_sig(),
            arb_sig(),
            any::<Vec<u8>>(),
            arb_extra(),
            prop::collection::btree_map(arb_path(), arb_change(), 0..4),
        )
            .prop_map(
                |(parents, root, author, committer, message, extra, changes)| Changeset {
                    parents: parents.into_iter().map(ChangesetId::from_bytes).collect(),
                    root: TreeId::from_bytes(root),
                    author,
                    committer,
                    message,
                    extra,
                    changes: SortedMap::new(changes.into_iter().collect()).unwrap(),
                },
            )
    }

    fn arb_tag() -> impl Strategy<Value = TagRecord> {
        (
            any::<[u8; 32]>(),
            any::<Vec<u8>>(),
            prop::option::of(arb_sig()),
            any::<Vec<u8>>(),
            arb_extra(),
        )
            .prop_map(|(t, name, tagger, message, extra)| TagRecord {
                target: ChangesetId::from_bytes(t),
                name,
                tagger,
                message,
                extra,
            })
    }

    fn arb_chunks() -> impl Strategy<Value = ChunkList> {
        prop::collection::vec(
            (
                any::<[u8; 32]>().prop_map(ChunkId::from_bytes),
                1..=MAX_CHUNK_LEN,
            ),
            2..5,
        )
        .prop_filter_map("size above threshold", |chunks| {
            let size: u64 = chunks.iter().map(|c| u64::from(c.1)).sum();
            (size > CHUNK_THRESHOLD).then_some(ChunkList { size, chunks })
        })
    }

    fn roundtrip<T: Enveloped + PartialEq + fmt::Debug>(x: &T) {
        let b = x.to_envelope();
        let y: T = decode_envelope(&b).unwrap();
        assert_eq!(&y, x);
        assert_eq!(y.to_envelope(), b);
    }

    proptest! {
        #[test]
        fn tree_roundtrip(x in arb_tree()) { roundtrip(&x); }
        #[test]
        fn changeset_roundtrip(x in arb_changeset()) { roundtrip(&x); }
        #[test]
        fn tag_roundtrip(x in arb_tag()) { roundtrip(&x); }
        #[test]
        fn chunklist_roundtrip(x in arb_chunks()) { roundtrip(&x); }

        /// encode(decode(b)) == b for every accepted b; arbitrary bytes are never mis-accepted.
        #[test]
        fn accepted_bytes_reencode(mut b in prop::collection::vec(any::<u8>(), 0..64)) {
            b.splice(0..0, [b'a', b's', 1, 1]);
            if let Ok(t) = decode_envelope::<Tree>(&b) {
                prop_assert_eq!(t.to_envelope(), b);
            }
        }
    }
}
