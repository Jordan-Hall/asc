//! First-class conflicts: the stored `ConflictSet` and the term algebra (SPEC §2.2 kind 7, §2.6
//! rule 7; docs/design/FIRST-CLASS-CONFLICTS.md "Data model").
//!
//! A conflicted changeset keeps an ordinary v1 root tree. The conflict itself lives in a
//! `ConflictSet` object that the changeset names through `extra["asc.conflicts"]`. Each conflicted
//! path holds an alternating sum of terms, as in jj:
//!
//! ```text
//! value = adds[0] - removes[0] + adds[1] - removes[1] + ... + adds[n]
//! ```
//!
//! This module defines the stored types, their exact encoding and every refusal the decoder makes.
//! It reads no tree and no file, so the checks that need a store (terms exist, `size` and `git`
//! match, the root agrees with `shown`) belong to ingest.

use std::fmt;
use std::marker::PhantomData;

use serde::de::{Error as _, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::codec::{CodecError, EnvelopeKind, Enveloped, decode_envelope};
use crate::id::{ConflictSetId, GitOid};
use crate::object::{Entry, Kind};
use crate::path::{Name, RepoPath};

/// Most adds one conflicted path may have (so at most 7 removes). Wider conflicts are not
/// recorded; the restack pauses as before. A decoder constant: raising it later is compatible
/// for readers that know the higher limit.
pub const MAX_CONFLICT_SIDES: usize = 8;
/// Most conflicted paths in one set.
pub const MAX_CONFLICT_ENTRIES: usize = 10_000;
/// Longest encoded `ConflictSet` envelope, checked before any length prefix is read.
pub const MAX_CONFLICT_SET_BYTES: usize = 4 * 1024 * 1024;
/// Largest single term the marker renderer reads; above it a path is shown as `Shown::Side`.
pub const MAX_RENDER_TERM: usize = 8 * 1024 * 1024;
/// Largest total of all terms of one path the marker renderer reads.
pub const MAX_RENDER_TOTAL: usize = 32 * 1024 * 1024;
/// Largest edited conflicted file that read-back parses; a larger one resolves only with `--mark`.
pub const MAX_READBACK_BYTES: usize = 64 * 1024 * 1024;
/// Shortest and longest marker run (`<<<<<<<` is 7).
pub const MARKER_LEN_MIN: u8 = 7;
pub const MARKER_LEN_MAX: u8 = 32;

/// Why a conflict set or one of its parts is not valid.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConflictError {
    #[error("conflict set is empty")]
    EmptySet,
    #[error("conflict set has more than {MAX_CONFLICT_ENTRIES} entries")]
    TooManyEntries,
    #[error("conflict set paths are not strictly ascending")]
    UnsortedPaths,
    #[error("conflict set is longer than {MAX_CONFLICT_SET_BYTES} bytes")]
    SetTooLarge,
    #[error("conflict has {0} adds; it needs 2..={MAX_CONFLICT_SIDES}")]
    Sides(usize),
    #[error("conflict has {adds} adds and {removes} removes; removes must be one fewer")]
    SideCount { adds: usize, removes: usize },
    #[error("a remove equals an add; the conflict is not simplified")]
    NotSimplified,
    #[error("all adds are equal; the conflict is resolved")]
    Resolved,
    #[error("conflict term is a directory")]
    DirTerm,
    #[error("gitlink term has a non-zero id or size")]
    GitlinkTerm,
    #[error("marker length is outside {MARKER_LEN_MIN}..={MARKER_LEN_MAX}")]
    MarkerLen,
    #[error("shown side index is out of range or names an absent term")]
    ShownSide,
    #[error("`from` is set on a conflict that is not a file/directory conflict")]
    FromKind,
}

/// Why a path could not merge cleanly. Stored kinds only: a case-fold pair is a land warning, not
/// a conflict in a tree (SPEC §4.4). The postcard variant index is the discriminant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ConflictKind {
    /// Both sides changed the file differently.
    Content = 0,
    /// Both sides added the path with different content.
    AddAdd = 1,
    /// One side deleted what the other changed.
    ModifyDelete = 2,
    /// One side has a directory where the other has a file.
    FileDir = 3,
    /// A binary file changed on both sides.
    Binary = 4,
}

/// One value of a path in one side of a conflict: absent, or a leaf entry.
///
/// A `Leaf` mirrors a tree `Entry` without the name: `id` is a `FileId`, or all zero for a
/// gitlink (whose `size` is then 0 and whose `git` is the submodule commit).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawTerm")]
pub enum Term {
    Absent,
    Leaf {
        /// File, Exec, Symlink or Gitlink; never Dir.
        kind: Kind,
        id: [u8; 32],
        size: u64,
        git: GitOid,
    },
}

#[derive(Deserialize)]
enum RawTerm {
    Absent,
    Leaf {
        kind: Kind,
        id: [u8; 32],
        size: u64,
        git: GitOid,
    },
}

impl TryFrom<RawTerm> for Term {
    type Error = ConflictError;

    fn try_from(raw: RawTerm) -> Result<Self, ConflictError> {
        let term = match raw {
            RawTerm::Absent => Term::Absent,
            RawTerm::Leaf {
                kind,
                id,
                size,
                git,
            } => Term::Leaf {
                kind,
                id,
                size,
                git,
            },
        };
        term.check()?;
        Ok(term)
    }
}

impl Term {
    /// The term for a tree entry; `None` for a directory, which is never a term.
    pub fn from_entry(entry: &Entry) -> Option<Self> {
        (entry.kind != Kind::Dir).then_some(Term::Leaf {
            kind: entry.kind,
            id: entry.id,
            size: entry.size,
            git: entry.git,
        })
    }

    /// The tree entry named `name` that this term stands for; `None` for `Absent`.
    pub fn to_entry(&self, name: Name) -> Option<Entry> {
        match *self {
            Term::Absent => None,
            Term::Leaf {
                kind,
                id,
                size,
                git,
            } => Some(Entry {
                name,
                kind,
                id,
                size,
                git,
            }),
        }
    }

    pub fn is_absent(&self) -> bool {
        matches!(self, Term::Absent)
    }

    fn check(&self) -> Result<(), ConflictError> {
        match self {
            Term::Absent => Ok(()),
            Term::Leaf {
                kind: Kind::Dir, ..
            } => Err(ConflictError::DirTerm),
            Term::Leaf {
                kind: Kind::Gitlink,
                id,
                size,
                ..
            } if *id != [0; 32] || *size != 0 => Err(ConflictError::GitlinkTerm),
            Term::Leaf { .. } => Ok(()),
        }
    }
}

/// What the root tree holds at a conflicted path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawShown")]
pub enum Shown {
    /// A text file with conflict markers of `marker_len` characters.
    Markers { marker_len: u8 },
    /// Exactly `adds[index]`.
    Side { index: u8 },
}

#[derive(Deserialize)]
enum RawShown {
    Markers { marker_len: u8 },
    Side { index: u8 },
}

impl TryFrom<RawShown> for Shown {
    type Error = ConflictError;

    fn try_from(raw: RawShown) -> Result<Self, ConflictError> {
        let shown = match raw {
            RawShown::Markers { marker_len } => Shown::Markers { marker_len },
            RawShown::Side { index } => Shown::Side { index },
        };
        if let Shown::Markers { marker_len } = shown
            && !(MARKER_LEN_MIN..=MARKER_LEN_MAX).contains(&marker_len)
        {
            return Err(ConflictError::MarkerLen);
        }
        Ok(shown)
    }
}

/// An alternating sum of terms with one more add than removes: the value algebra of a path.
///
/// `TermSum` is not limited to [`MAX_CONFLICT_SIDES`], because a flattened sum can pass the limit
/// before it simplifies; [`Conflicted::from_sum`] applies the limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermSum {
    adds: Vec<Term>,
    removes: Vec<Term>,
}

impl TermSum {
    /// A sum of `adds` and `removes`; `adds` must have exactly one more element.
    pub fn new(adds: Vec<Term>, removes: Vec<Term>) -> Result<Self, ConflictError> {
        if adds.len() != removes.len() + 1 {
            return Err(ConflictError::SideCount {
                adds: adds.len(),
                removes: removes.len(),
            });
        }
        Ok(Self { adds, removes })
    }

    /// The sum of one term: an ordinary value.
    pub fn single(term: Term) -> Self {
        Self {
            adds: vec![term],
            removes: Vec::new(),
        }
    }

    pub fn adds(&self) -> &[Term] {
        &self.adds
    }

    pub fn removes(&self) -> &[Term] {
        &self.removes
    }

    /// `onto - parent + change`, flattened and not simplified, in this order:
    /// `adds = onto.adds ++ parent.removes ++ change.adds` and
    /// `removes = onto.removes ++ parent.adds ++ change.removes`. For three plain values this is
    /// `adds = [onto, change]`, `removes = [parent]`, so `adds[0]` is the destination side.
    pub fn flatten(onto: &Self, parent: &Self, change: &Self) -> Self {
        let cat = |a: &[Term], b: &[Term], c: &[Term]| [a, b, c].concat();
        Self {
            adds: cat(&onto.adds, &parent.removes, &change.adds),
            removes: cat(&onto.removes, &parent.adds, &change.removes),
        }
    }

    /// Cancels pairs: scanning `removes` in order, each remove cancels the first equal add.
    pub fn simplify(mut self) -> Self {
        let mut i = 0;
        while i < self.removes.len() {
            match self.adds.iter().position(|a| *a == self.removes[i]) {
                Some(j) => {
                    self.adds.remove(j);
                    self.removes.remove(i);
                }
                None => i += 1,
            }
        }
        self
    }

    /// Rebases the value `change` from parent value `parent` onto `onto`: the simplified
    /// `onto - parent + change`.
    pub fn rebase(onto: &Self, parent: &Self, change: &Self) -> Self {
        Self::flatten(onto, parent, change).simplify()
    }

    /// The single value this sum resolves to: its one add, or the term all adds share.
    pub fn resolution(&self) -> Option<&Term> {
        let first = self.adds.first()?;
        self.adds.iter().all(|a| a == first).then_some(first)
    }
}

/// One conflicted path: its kind, its terms and how the root tree shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawConflicted")]
pub struct Conflicted {
    pub kind: ConflictKind,
    /// `2..=MAX_CONFLICT_SIDES` terms; `adds[0]` is the destination side.
    pub adds: Vec<Term>,
    /// One fewer than `adds`.
    pub removes: Vec<Term>,
    /// What the root tree holds at the path.
    pub shown: Shown,
    /// `FileDir` only: where the file was before the ort-style move.
    pub from: Option<RepoPath>,
}

#[derive(Deserialize)]
struct RawConflicted {
    kind: ConflictKind,
    #[serde(deserialize_with = "bounded_terms")]
    adds: Vec<Term>,
    #[serde(deserialize_with = "bounded_terms")]
    removes: Vec<Term>,
    shown: Shown,
    from: Option<RepoPath>,
}

impl TryFrom<RawConflicted> for Conflicted {
    type Error = ConflictError;

    fn try_from(r: RawConflicted) -> Result<Self, ConflictError> {
        Conflicted::new(r.kind, r.adds, r.removes, r.shown, r.from)
    }
}

impl Conflicted {
    /// A conflicted path, checked against every rule of the decoder.
    pub fn new(
        kind: ConflictKind,
        adds: Vec<Term>,
        removes: Vec<Term>,
        shown: Shown,
        from: Option<RepoPath>,
    ) -> Result<Self, ConflictError> {
        let c = Self {
            kind,
            adds,
            removes,
            shown,
            from,
        };
        c.validate()?;
        Ok(c)
    }

    /// A conflicted path from a sum that is a real, simplified conflict within the side limit.
    pub fn from_sum(
        sum: TermSum,
        kind: ConflictKind,
        shown: Shown,
        from: Option<RepoPath>,
    ) -> Result<Self, ConflictError> {
        Self::new(kind, sum.adds, sum.removes, shown, from)
    }

    /// The terms as a sum.
    pub fn sum(&self) -> TermSum {
        TermSum {
            adds: self.adds.clone(),
            removes: self.removes.clone(),
        }
    }

    /// Number of sides (adds).
    pub fn sides(&self) -> usize {
        self.adds.len()
    }

    /// The invariants of one entry; the decoder runs exactly these.
    pub fn validate(&self) -> Result<(), ConflictError> {
        let n = self.adds.len();
        if !(2..=MAX_CONFLICT_SIDES).contains(&n) {
            return Err(ConflictError::Sides(n));
        }
        if self.removes.len() != n - 1 {
            return Err(ConflictError::SideCount {
                adds: n,
                removes: self.removes.len(),
            });
        }
        for t in self.adds.iter().chain(&self.removes) {
            t.check()?;
        }
        if self.removes.iter().any(|r| self.adds.contains(r)) {
            return Err(ConflictError::NotSimplified);
        }
        if self.adds.iter().all(|a| *a == self.adds[0]) {
            return Err(ConflictError::Resolved);
        }
        match self.shown {
            Shown::Markers { marker_len } => {
                if !(MARKER_LEN_MIN..=MARKER_LEN_MAX).contains(&marker_len) {
                    return Err(ConflictError::MarkerLen);
                }
            }
            Shown::Side { index } => match self.adds.get(usize::from(index)) {
                Some(t) if !t.is_absent() => {}
                _ => return Err(ConflictError::ShownSide),
            },
        }
        if self.from.is_some() && self.kind != ConflictKind::FileDir {
            return Err(ConflictError::FromKind);
        }
        Ok(())
    }
}

/// The conflicted paths of one changeset, strictly ascending by path and never empty.
///
/// The field is private so every instance satisfies the invariants. The `ConflictSetId` is the
/// BLAKE3 id of the envelope; a changeset names it in `extra["asc.conflicts"]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawConflictSet")]
pub struct ConflictSet {
    entries: Vec<(RepoPath, Conflicted)>,
}

#[derive(Deserialize)]
struct RawConflictSet {
    #[serde(deserialize_with = "bounded_entries")]
    entries: Vec<(RepoPath, Conflicted)>,
}

impl TryFrom<RawConflictSet> for ConflictSet {
    type Error = ConflictError;

    /// The wire form must already be sorted. Each entry was checked when it decoded, and the
    /// envelope length cap bounds the whole set, so only the set-level rules run here.
    fn try_from(raw: RawConflictSet) -> Result<Self, ConflictError> {
        check_entries(&raw.entries)?;
        Ok(Self {
            entries: raw.entries,
        })
    }
}

fn check_entries(entries: &[(RepoPath, Conflicted)]) -> Result<(), ConflictError> {
    if entries.is_empty() {
        return Err(ConflictError::EmptySet);
    }
    if entries.len() > MAX_CONFLICT_ENTRIES {
        return Err(ConflictError::TooManyEntries);
    }
    if !entries.windows(2).all(|w| w[0].0 < w[1].0) {
        return Err(ConflictError::UnsortedPaths);
    }
    Ok(())
}

impl ConflictSet {
    /// Sorts `entries` by path and checks every rule. Duplicate paths, an empty set, more than
    /// [`MAX_CONFLICT_ENTRIES`] entries, an invalid entry and an encoding longer than
    /// [`MAX_CONFLICT_SET_BYTES`] are errors.
    pub fn new(mut entries: Vec<(RepoPath, Conflicted)>) -> Result<Self, ConflictError> {
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        check_entries(&entries)?;
        for (_, c) in &entries {
            c.validate()?;
        }
        let set = Self { entries };
        if set.to_envelope().len() > MAX_CONFLICT_SET_BYTES {
            return Err(ConflictError::SetTooLarge);
        }
        Ok(set)
    }

    /// The entries, strictly ascending by path.
    pub fn entries(&self) -> &[(RepoPath, Conflicted)] {
        &self.entries
    }

    pub fn get(&self, path: &RepoPath) -> Option<&Conflicted> {
        self.entries
            .binary_search_by(|e| e.0.cmp(path))
            .ok()
            .map(|i| &self.entries[i].1)
    }

    /// Number of conflicted paths; never zero.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn id(&self) -> ConflictSetId {
        ConflictSetId::of(&self.to_envelope())
    }

    /// Decodes a stored or received envelope, refusing every malformed form.
    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        decode_envelope(bytes)
    }
}

impl Enveloped for ConflictSet {
    const KIND: EnvelopeKind = EnvelopeKind::ConflictSet;
    const MAX_ENVELOPE_LEN: usize = MAX_CONFLICT_SET_BYTES;
}

// ---- bounded sequences ----

/// Reads a sequence of at most `max` elements. The length prefix is compared with `max` before
/// anything is allocated, and the element loop stops at `max + 1`, so a prefix cannot ask for
/// more memory than the limit allows.
fn bounded_seq<'de, D, T>(d: D, max: usize) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Bounded<T>(usize, PhantomData<T>);

    impl<'de, T: Deserialize<'de>> Visitor<'de> for Bounded<T> {
        type Value = Vec<T>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "a sequence of at most {} elements", self.0)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
            if seq.size_hint().is_some_and(|n| n > self.0) {
                return Err(A::Error::custom("sequence is longer than its limit"));
            }
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(self.0));
            while let Some(item) = seq.next_element()? {
                if out.len() == self.0 {
                    return Err(A::Error::custom("sequence is longer than its limit"));
                }
                out.push(item);
            }
            Ok(out)
        }
    }

    d.deserialize_seq(Bounded(max, PhantomData))
}

fn bounded_terms<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Term>, D::Error> {
    bounded_seq(d, MAX_CONFLICT_SIDES)
}

fn bounded_entries<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Vec<(RepoPath, Conflicted)>, D::Error> {
    bounded_seq(d, MAX_CONFLICT_ENTRIES)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;

    use super::*;
    use crate::codec::encode_envelope;

    fn path(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    /// A file term whose bytes follow `b`: id `[b; 32]`, size `b`, git `[b + 100; 20]`.
    fn file(b: u8) -> Term {
        Term::Leaf {
            kind: Kind::File,
            id: [b; 32],
            size: u64::from(b),
            git: GitOid::from_bytes([b.wrapping_add(100); 20]),
        }
    }

    fn two_sided() -> Conflicted {
        two_sided_with(1, 2, 3)
    }

    fn two_sided_with(a: u8, b: u8, base: u8) -> Conflicted {
        Conflicted::new(
            ConflictKind::Content,
            vec![file(a), file(b)],
            vec![file(base)],
            Shown::Markers { marker_len: 7 },
            None,
        )
        .unwrap()
    }

    fn one(c: Conflicted) -> Vec<(RepoPath, Conflicted)> {
        vec![(path("f"), c)]
    }

    /// The wire form of a set whose entries are not checked: what a hostile writer could send.
    fn wire(entries: &[(RepoPath, Conflicted)]) -> Vec<u8> {
        #[derive(Serialize)]
        struct Wire<'a> {
            entries: &'a [(RepoPath, Conflicted)],
        }
        encode_envelope(EnvelopeKind::ConflictSet, 1, &Wire { entries })
    }

    fn refused(bytes: &[u8]) {
        assert!(
            matches!(ConflictSet::decode(bytes), Err(CodecError::Payload(_))),
            "{bytes:?}"
        );
    }

    // ---- golden ----

    /// A two-sided text conflict, a modify/delete shown as its first side, and a three-sided
    /// file/directory conflict with one term of every kind.
    fn golden_set() -> ConflictSet {
        let exec = Term::Leaf {
            kind: Kind::Exec,
            id: [4; 32],
            size: 1,
            git: GitOid::from_bytes([5; 20]),
        };
        let link = Term::Leaf {
            kind: Kind::Symlink,
            id: [6; 32],
            size: 2,
            git: GitOid::from_bytes([7; 20]),
        };
        let gitlink = Term::Leaf {
            kind: Kind::Gitlink,
            id: [0; 32],
            size: 0,
            git: GitOid::from_bytes([8; 20]),
        };
        ConflictSet::new(vec![
            (
                path("dir/gone.txt"),
                Conflicted::new(
                    ConflictKind::ModifyDelete,
                    vec![file(1), Term::Absent],
                    vec![file(2)],
                    Shown::Side { index: 0 },
                    None,
                )
                .unwrap(),
            ),
            (path("a.txt"), two_sided()),
            (
                path("z"),
                Conflicted::new(
                    ConflictKind::FileDir,
                    vec![exec, link, gitlink],
                    vec![Term::Absent, file(9)],
                    Shown::Side { index: 1 },
                    Some(path("y")),
                )
                .unwrap(),
            ),
        ])
        .unwrap()
    }

    /// `Term::Leaf` by hand: variant 1, `Kind` index, 32 raw id bytes, varint size, 20 raw git
    /// bytes. Sizes here are below 128, so one varint byte.
    fn leaf(kind: u8, id: u8, size: u8, git: u8) -> Vec<u8> {
        let mut v = vec![1, kind];
        v.extend([id; 32]);
        v.push(size);
        v.extend([git; 20]);
        v
    }

    /// One entry by hand: path (varint length, bytes), kind index, adds (length, terms), removes
    /// (length, terms), shown (variant, byte), from (`Option` tag, then path).
    fn entry(
        p: &[u8],
        kind: u8,
        adds: &[Vec<u8>],
        removes: &[Vec<u8>],
        shown: [u8; 2],
        from: Option<&[u8]>,
    ) -> Vec<u8> {
        let mut v = vec![u8::try_from(p.len()).unwrap()];
        v.extend(p);
        v.push(kind);
        v.push(u8::try_from(adds.len()).unwrap());
        v.extend(adds.concat());
        v.push(u8::try_from(removes.len()).unwrap());
        v.extend(removes.concat());
        v.extend(shown);
        match from {
            None => v.push(0),
            Some(f) => {
                v.push(1);
                v.push(u8::try_from(f.len()).unwrap());
                v.extend(f);
            }
        }
        v
    }

    /// The bytes of [`golden_set`], assembled by hand from the postcard rules of SPEC §2.2 and
    /// not through serde.
    fn golden_by_hand() -> Vec<u8> {
        let absent = vec![0u8];
        let mut gitlink = vec![1, 4];
        gitlink.extend([0u8; 32]);
        gitlink.push(0);
        gitlink.extend([8u8; 20]);
        // Magic "as", kind 7, version 1, then the entry count.
        let mut out = vec![b'a', b's', 7, 1, 3];
        out.extend(entry(
            b"a.txt",
            0, // Content
            &[leaf(0, 1, 1, 101), leaf(0, 2, 2, 102)],
            &[leaf(0, 3, 3, 103)],
            [0, 7], // Markers { marker_len: 7 }
            None,
        ));
        out.extend(entry(
            b"dir/gone.txt",
            2, // ModifyDelete
            &[leaf(0, 1, 1, 101), absent.clone()],
            &[leaf(0, 2, 2, 102)],
            [1, 0], // Side { index: 0 }
            None,
        ));
        out.extend(entry(
            b"z",
            3, // FileDir
            &[leaf(1, 4, 1, 5), leaf(2, 6, 2, 7), gitlink],
            &[absent, leaf(0, 9, 9, 109)],
            [1, 1], // Side { index: 1 }
            Some(b"y"),
        ));
        out
    }

    fn golden_file(name: &str) -> String {
        let file = format!(
            "{}/tests/golden/envelopes/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("{file}: {e}"))
            .trim()
            .to_owned()
    }

    #[test]
    fn conflict_golden_encoding_matches_hand_assembly() {
        let set = golden_set();
        assert_eq!(set.to_envelope(), golden_by_hand());
        assert_eq!(ConflictSet::decode(&golden_by_hand()).unwrap(), set);
    }

    #[test]
    fn conflict_golden_hex_and_id_are_frozen() {
        let set = golden_set();
        let hex = data_encoding::HEXLOWER.encode(&set.to_envelope());
        let id = set.id().to_string();
        if std::env::var_os("ASC_BLESS").is_some() {
            let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/envelopes");
            std::fs::write(format!("{dir}/conflictset.hex"), format!("{hex}\n")).unwrap();
            std::fs::write(format!("{dir}/conflictset_id.txt"), format!("{id}\n")).unwrap();
        }
        assert_eq!(hex, golden_file("conflictset.hex"));
        assert_eq!(id, golden_file("conflictset_id.txt"));
    }

    #[test]
    fn conflict_refuses_an_empty_set() {
        assert_eq!(
            ConflictSet::try_from(RawConflictSet { entries: vec![] }).unwrap_err(),
            ConflictError::EmptySet
        );
        assert_eq!(
            ConflictSet::new(vec![]).unwrap_err(),
            ConflictError::EmptySet
        );
        refused(&[b'a', b's', 7, 1, 0]);
    }

    // ---- decoder refusals: entry rules ----

    /// Breaks `two_sided()` with `f`; both the constructors and the decoder must refuse it.
    fn bad(f: impl FnOnce(&mut Conflicted), want: ConflictError) {
        let mut c = two_sided();
        f(&mut c);
        assert_eq!(c.validate().unwrap_err(), want);
        assert_eq!(
            ConflictSet::new(one(c.clone())).unwrap_err(),
            want,
            "ConflictSet::new"
        );
        refused(&wire(&one(c)));
    }

    #[test]
    fn conflict_refuses_wrong_side_counts() {
        bad(|c| c.adds.truncate(1), ConflictError::Sides(1));
        bad(
            |c| {
                c.adds = (1..=9).map(file).collect();
                c.removes = (11..=18).map(file).collect();
            },
            ConflictError::Sides(9),
        );
        bad(
            |c| c.removes.clear(),
            ConflictError::SideCount {
                adds: 2,
                removes: 0,
            },
        );
        bad(
            |c| c.removes.push(file(4)),
            ConflictError::SideCount {
                adds: 2,
                removes: 2,
            },
        );
    }

    #[test]
    fn conflict_accepts_exactly_the_side_limit() {
        let c = Conflicted::new(
            ConflictKind::Content,
            (1..=8).map(file).collect(),
            (11..=17).map(file).collect(),
            Shown::Markers { marker_len: 32 },
            None,
        )
        .unwrap();
        let set = ConflictSet::new(one(c)).unwrap();
        assert_eq!(ConflictSet::decode(&set.to_envelope()).unwrap(), set);
    }

    #[test]
    fn conflict_refuses_unsimplified_and_resolved() {
        // A remove equal to either add, or to Absent when an add is Absent.
        bad(|c| c.removes[0] = file(1), ConflictError::NotSimplified);
        bad(|c| c.removes[0] = file(2), ConflictError::NotSimplified);
        bad(
            |c| {
                c.adds[1] = Term::Absent;
                c.removes[0] = Term::Absent;
                c.shown = Shown::Markers { marker_len: 7 };
            },
            ConflictError::NotSimplified,
        );
        // All adds equal, including all Absent.
        bad(|c| c.adds[1] = file(1), ConflictError::Resolved);
        bad(
            |c| {
                c.adds = vec![Term::Absent, Term::Absent];
                c.shown = Shown::Markers { marker_len: 7 };
            },
            ConflictError::Resolved,
        );
    }

    #[test]
    fn conflict_refuses_bad_terms() {
        let dir = Term::Leaf {
            kind: Kind::Dir,
            id: [1; 32],
            size: 0,
            git: GitOid::from_bytes([1; 20]),
        };
        bad(|c| c.adds[0] = dir, ConflictError::DirTerm);
        bad(|c| c.removes[0] = dir, ConflictError::DirTerm);
        let gitlink = |id: [u8; 32], size: u64| Term::Leaf {
            kind: Kind::Gitlink,
            id,
            size,
            git: GitOid::from_bytes([1; 20]),
        };
        bad(
            |c| c.adds[0] = gitlink([1; 32], 0),
            ConflictError::GitlinkTerm,
        );
        bad(
            |c| c.adds[0] = gitlink([0; 32], 1),
            ConflictError::GitlinkTerm,
        );
    }

    #[test]
    fn conflict_refuses_bad_shown() {
        for len in [0, 1, 6, 33, 255] {
            bad(
                |c| c.shown = Shown::Markers { marker_len: len },
                ConflictError::MarkerLen,
            );
        }
        for len in [7, 8, 31, 32] {
            let mut c = two_sided();
            c.shown = Shown::Markers { marker_len: len };
            assert_eq!(c.validate(), Ok(()), "{len}");
        }
        bad(
            |c| c.shown = Shown::Side { index: 2 },
            ConflictError::ShownSide,
        );
        bad(
            |c| c.shown = Shown::Side { index: 255 },
            ConflictError::ShownSide,
        );
        bad(
            |c| {
                c.adds[1] = Term::Absent;
                c.shown = Shown::Side { index: 1 };
            },
            ConflictError::ShownSide,
        );
    }

    #[test]
    fn conflict_refuses_from_on_other_kinds() {
        for kind in [
            ConflictKind::Content,
            ConflictKind::AddAdd,
            ConflictKind::ModifyDelete,
            ConflictKind::Binary,
        ] {
            bad(
                |c| {
                    c.kind = kind;
                    c.from = Some(path("y"));
                },
                ConflictError::FromKind,
            );
        }
        let mut c = two_sided();
        c.kind = ConflictKind::FileDir;
        c.from = Some(path("y"));
        assert_eq!(c.validate(), Ok(()));
    }

    // ---- decoder refusals: set rules and envelope ----

    #[test]
    fn conflict_refuses_unsorted_and_duplicate_paths() {
        let a = (path("a"), two_sided());
        let b = (path("b"), two_sided());
        assert_eq!(
            ConflictSet::try_from(RawConflictSet {
                entries: vec![b.clone(), a.clone()]
            })
            .unwrap_err(),
            ConflictError::UnsortedPaths
        );
        refused(&wire(&[b.clone(), a.clone()]));
        refused(&wire(&[a.clone(), a.clone()]));
        assert_eq!(
            ConflictSet::new(vec![a.clone(), a.clone()]).unwrap_err(),
            ConflictError::UnsortedPaths
        );
        // `new` sorts instead of refusing.
        let set = ConflictSet::new(vec![b, a]).unwrap();
        assert_eq!(set.entries()[0].0, path("a"));
        assert!(set.get(&path("b")).is_some() && set.get(&path("c")).is_none());
        assert_eq!(set.len(), 2);
        assert!(!set.is_empty());
    }

    #[test]
    fn conflict_refuses_invalid_paths() {
        // Paths with an empty component, a `..` component and a `.git` component.
        let tail = entry(
            b"f",
            0,
            &[leaf(0, 1, 1, 101), leaf(0, 2, 2, 102)],
            &[leaf(0, 3, 3, 103)],
            [0, 7],
            None,
        );
        for p in [&b"a//b"[..], b"..", b"a/.git", b"/a"] {
            let mut bytes = vec![b'a', b's', 7, 1, 1, u8::try_from(p.len()).unwrap()];
            bytes.extend(p);
            bytes.extend(&tail[2..]);
            refused(&bytes);
        }
        // The same bytes with a valid path decode.
        let mut ok = vec![b'a', b's', 7, 1, 1];
        ok.extend(&tail);
        assert!(ConflictSet::decode(&ok).is_ok());
    }

    fn entries_of(n: usize) -> Vec<(RepoPath, Conflicted)> {
        (0..n)
            .map(|i| (path(&format!("p{i:05}")), two_sided()))
            .collect()
    }

    #[test]
    fn conflict_entry_limit() {
        let set = ConflictSet::new(entries_of(MAX_CONFLICT_ENTRIES)).unwrap();
        assert!(set.to_envelope().len() <= MAX_CONFLICT_SET_BYTES);
        assert_eq!(ConflictSet::decode(&set.to_envelope()).unwrap(), set);
        let over = entries_of(MAX_CONFLICT_ENTRIES + 1);
        assert_eq!(
            ConflictSet::new(over.clone()).unwrap_err(),
            ConflictError::TooManyEntries
        );
        refused(&wire(&over));
    }

    #[test]
    fn conflict_length_prefix_is_checked_before_reading() {
        // A prefix of 10_001 entries and nothing after it.
        refused(&[b'a', b's', 7, 1, 0xF1, 0x4E]);
        // A prefix near 2^63 with no data; the decoder must not try to allocate it.
        let mut huge = vec![b'a', b's', 7, 1];
        huge.extend([0xFF; 8]);
        huge.push(0x7F);
        refused(&huge);
        // An adds length of 9 with the nine terms present.
        let mut adds9 = vec![b'a', b's', 7, 1, 1, 1, b'f', 0, 9];
        adds9.extend([0u8; 9]);
        refused(&adds9);
    }

    #[test]
    fn conflict_size_limit() {
        let mut over = vec![b'a', b's', 7, 1];
        over.resize(MAX_CONFLICT_SET_BYTES + 1, 0);
        assert_eq!(
            ConflictSet::decode(&over),
            Err(CodecError::TooLarge {
                max: MAX_CONFLICT_SET_BYTES
            })
        );
        // `new` refuses to build a set that the decoder would refuse.
        let long = RepoPath::new(vec![b'a'; MAX_CONFLICT_SET_BYTES]).unwrap();
        assert_eq!(
            ConflictSet::new(vec![(long, two_sided())]).unwrap_err(),
            ConflictError::SetTooLarge
        );
    }

    #[test]
    fn conflict_refuses_malformed_envelopes() {
        let good = ConflictSet::new(one(two_sided())).unwrap().to_envelope();
        assert!(ConflictSet::decode(&good).is_ok());
        let mut m = good.clone();
        m[0] = b'x';
        assert_eq!(ConflictSet::decode(&m), Err(CodecError::BadMagic));
        let mut m = good.clone();
        m[2] = 1;
        assert!(matches!(
            ConflictSet::decode(&m),
            Err(CodecError::WrongKind { .. })
        ));
        let mut m = good.clone();
        m[3] = 2;
        assert!(matches!(
            ConflictSet::decode(&m),
            Err(CodecError::UnknownVersion { version: 2, .. })
        ));
        let mut m = good.clone();
        m.push(0);
        assert_eq!(ConflictSet::decode(&m), Err(CodecError::Trailing));
        refused(&good[..good.len() - 1]);
        // A tree decoder never accepts a conflict set.
        assert!(decode_envelope::<crate::object::Tree>(&good).is_err());
    }

    #[test]
    fn conflict_refuses_non_canonical_encodings() {
        let good = ConflictSet::new(one(two_sided())).unwrap().to_envelope();
        // Offsets: 0..4 header, 4 entry count, 5 path length, 6 path, 7 kind, 8 adds length,
        // 9 first term variant, 10 first term Kind, 11..43 id, 43 size, 44..64 git.
        assert_eq!(&good[..11], &[b'a', b's', 7, 1, 1, 1, b'f', 0, 2, 1, 0]);
        // Entry count 1 as a two-byte and as a three-byte varint.
        for count in [&[0x81u8, 0x00][..], &[0x81, 0x80, 0x00]] {
            let mut m = good[..4].to_vec();
            m.extend(count);
            m.extend(&good[5..]);
            assert!(ConflictSet::decode(&m).is_err());
        }
        // Size 1 as a two-byte varint.
        assert_eq!(good[43], 1);
        let mut m = good[..43].to_vec();
        m.extend([0x81, 0x00]);
        m.extend(&good[44..]);
        assert!(ConflictSet::decode(&m).is_err());
        // `Option` tag other than 0 or 1.
        let mut m = good.clone();
        let last = m.len() - 1;
        m[last] = 2;
        assert!(ConflictSet::decode(&m).is_err());
        // Unknown variants: ConflictKind 5, Term 2, Kind 5; and Kind Dir.
        for (at, v) in [(7, 5u8), (9, 2), (10, 5), (10, 3)] {
            let mut m = good.clone();
            m[at] = v;
            assert!(ConflictSet::decode(&m).is_err());
        }
    }

    // ---- ids and accessors ----

    #[test]
    fn conflict_set_id_uses_its_own_context() {
        let set = golden_set();
        let env = set.to_envelope();
        assert_eq!(set.id(), ConflictSetId::of(&env));
        let derived = crate::id::derive(crate::id::context::CONFLICT_SET, &env);
        assert_eq!(set.id().as_bytes(), &derived);
        assert_ne!(set.id().as_bytes(), blake3::hash(&env).as_bytes());
        assert_ne!(
            set.id().as_bytes(),
            &crate::id::derive(crate::id::context::TREE, &env)
        );
    }

    #[test]
    fn conflict_term_entry_round_trip() {
        let e = Entry {
            name: Name::new("f").unwrap(),
            kind: Kind::Exec,
            id: [3; 32],
            size: 9,
            git: GitOid::from_bytes([4; 20]),
        };
        let t = Term::from_entry(&e).unwrap();
        assert_eq!(t.to_entry(e.name.clone()), Some(e.clone()));
        assert_eq!(Term::Absent.to_entry(e.name.clone()), None);
        let dir = Entry {
            kind: Kind::Dir,
            ..e
        };
        assert_eq!(Term::from_entry(&dir), None);
    }

    // ---- term algebra ----

    fn sum(adds: &[Term], removes: &[Term]) -> TermSum {
        TermSum::new(adds.to_vec(), removes.to_vec()).unwrap()
    }

    #[test]
    fn conflict_sum_needs_one_more_add() {
        assert_eq!(
            TermSum::new(vec![file(1)], vec![file(2)]).unwrap_err(),
            ConflictError::SideCount {
                adds: 1,
                removes: 1
            }
        );
        assert!(TermSum::new(vec![], vec![]).is_err());
    }

    #[test]
    fn conflict_rebase_of_plain_values_is_two_sided() {
        let r = TermSum::rebase(
            &TermSum::single(file(1)),
            &TermSum::single(file(3)),
            &TermSum::single(file(2)),
        );
        assert_eq!(r, sum(&[file(1), file(2)], &[file(3)]));
        assert_eq!(r.resolution(), None);
        // `Conflicted::sum` and `from_sum` are inverse.
        let c = Conflicted::from_sum(
            r,
            ConflictKind::Content,
            Shown::Markers { marker_len: 7 },
            None,
        )
        .unwrap();
        assert_eq!(c, two_sided());
        assert_eq!(c.sum(), sum(&[file(1), file(2)], &[file(3)]));
        assert_eq!(c.sides(), 2);
    }

    #[test]
    fn conflict_unchanged_path_takes_the_new_parent() {
        // The change did not touch the path: change == parent, so the result is `onto`.
        let r = TermSum::rebase(
            &TermSum::single(file(1)),
            &TermSum::single(file(3)),
            &TermSum::single(file(3)),
        );
        assert_eq!(r, TermSum::single(file(1)));
        assert_eq!(r.resolution(), Some(&file(1)));
    }

    #[test]
    fn conflict_resolution_in_the_parent_cleans_the_child() {
        // C1 and C2 are both conflicted at f, and C2 did not edit f, so C2.f == C1.f.
        let c1 = sum(&[file(1), file(2)], &[file(3)]);
        let x = TermSum::single(file(7));
        let flat = TermSum::flatten(&x, &c1, &c1);
        assert_eq!(flat.adds().len(), 4);
        assert_eq!(flat.removes().len(), 3);
        let r = flat.simplify();
        assert_eq!(r, x);
        assert_eq!(r.resolution(), Some(&file(7)));
    }

    #[test]
    fn conflict_edited_child_keeps_only_its_changes() {
        // C2 edited f to a conflict that shares side 1 and the base with C1.f.
        let c1 = sum(&[file(1), file(2)], &[file(3)]);
        let c2 = sum(&[file(1), file(4)], &[file(3)]);
        let x = TermSum::single(file(7));
        // x - (1 - 3 + 2) + (1 - 3 + 4) = x - 2 + 4
        assert_eq!(
            TermSum::rebase(&x, &c1, &c2),
            sum(&[file(7), file(4)], &[file(2)])
        );
    }

    #[test]
    fn conflict_flatten_keeps_the_destination_first() {
        let onto = sum(&[file(1), file(2)], &[file(3)]);
        let parent = TermSum::single(file(4));
        let change = TermSum::single(file(5));
        let f = TermSum::flatten(&onto, &parent, &change);
        assert_eq!(f.adds(), &[file(1), file(2), file(5)]);
        assert_eq!(f.removes(), &[file(3), file(4)]);
    }

    #[test]
    fn conflict_simplify_cancels_the_first_equal_add_in_remove_order() {
        // Two removes of 2 cancel two of the three adds of 2; the last stays.
        let s = sum(&[file(2), file(2), file(2)], &[file(2), file(2)]).simplify();
        assert_eq!(s, TermSum::single(file(2)));
        // The first equal add goes, so the later one keeps its place.
        let s = sum(&[file(1), file(2), file(1)], &[file(1), file(5)]).simplify();
        assert_eq!(s, sum(&[file(2), file(1)], &[file(5)]));
        // A remove with no equal add stays.
        let s = sum(&[file(1), file(2)], &[file(3)]).simplify();
        assert_eq!(s, sum(&[file(1), file(2)], &[file(3)]));
        // Absent cancels Absent.
        let s = sum(&[Term::Absent, file(1)], &[Term::Absent]).simplify();
        assert_eq!(s, TermSum::single(file(1)));
    }

    #[test]
    fn conflict_all_adds_equal_is_a_resolution() {
        // 2a - b does not simplify, but it resolves to a.
        let s = sum(&[file(1), file(1)], &[file(2)]).simplify();
        assert_eq!(s.adds().len(), 2);
        assert_eq!(s.resolution(), Some(&file(1)));
        // The decoder refuses to store it.
        assert_eq!(
            Conflicted::from_sum(
                s,
                ConflictKind::Content,
                Shown::Markers { marker_len: 7 },
                None
            ),
            Err(ConflictError::Resolved)
        );
    }

    // ---- properties ----

    fn term() -> impl Strategy<Value = Term> {
        prop_oneof![Just(Term::Absent), (0u8..5).prop_map(file)]
    }

    fn any_sum() -> impl Strategy<Value = TermSum> {
        (1usize..7)
            .prop_flat_map(|n| {
                (
                    prop::collection::vec(term(), n),
                    prop::collection::vec(term(), n - 1),
                )
            })
            .prop_map(|(a, r)| TermSum::new(a, r).unwrap())
    }

    type Net = BTreeMap<String, i32>;

    /// The signed count of each term: an add counts +1, a remove -1.
    fn net(s: &TermSum) -> Net {
        let mut m = Net::new();
        for t in s.adds() {
            *m.entry(format!("{t:?}")).or_insert(0) += 1;
        }
        for t in s.removes() {
            *m.entry(format!("{t:?}")).or_insert(0) -= 1;
        }
        m.retain(|_, v| *v != 0);
        m
    }

    fn combine(a: &Net, b: &Net, sign: i32) -> Net {
        let mut m = a.clone();
        for (k, v) in b {
            *m.entry(k.clone()).or_insert(0) += sign * v;
        }
        m.retain(|_, v| *v != 0);
        m
    }

    fn any_conflicted() -> impl Strategy<Value = Conflicted> {
        any_sum().prop_filter_map("valid conflict", |s| {
            let s = s.simplify();
            let index = s.adds().iter().position(|t| !t.is_absent())?;
            Conflicted::from_sum(
                s,
                ConflictKind::Content,
                Shown::Side {
                    index: u8::try_from(index).unwrap(),
                },
                None,
            )
            .ok()
        })
    }

    fn numbered(cs: Vec<Conflicted>) -> ConflictSet {
        let entries = cs
            .into_iter()
            .enumerate()
            .map(|(i, c)| (path(&format!("p{i}")), c))
            .collect();
        ConflictSet::new(entries).unwrap()
    }

    proptest! {
        #[test]
        fn conflict_simplify_keeps_value_and_shape(s in any_sum()) {
            let before = net(&s);
            let t = s.simplify();
            prop_assert_eq!(t.adds().len(), t.removes().len() + 1);
            prop_assert_eq!(net(&t), before);
            prop_assert!(t.removes().iter().all(|r| !t.adds().contains(r)));
            prop_assert_eq!(t.clone().simplify(), t);
        }

        #[test]
        fn conflict_flatten_is_onto_minus_parent_plus_change(
            o in any_sum(), p in any_sum(), c in any_sum()
        ) {
            let f = TermSum::flatten(&o, &p, &c);
            prop_assert_eq!(f.adds().len(), f.removes().len() + 1);
            let want = combine(&combine(&net(&o), &net(&p), -1), &net(&c), 1);
            prop_assert_eq!(net(&f), want.clone());
            prop_assert_eq!(net(&f.simplify()), want);
        }

        #[test]
        fn conflict_decode_inverts_encode(cs in prop::collection::vec(any_conflicted(), 1..5)) {
            let set = numbered(cs);
            let bytes = set.to_envelope();
            let back = ConflictSet::decode(&bytes).unwrap();
            prop_assert_eq!(&back, &set);
            prop_assert_eq!(back.to_envelope(), bytes);
        }

        #[test]
        fn conflict_decode_accepts_only_canonical_forms(
            cs in prop::collection::vec(any_conflicted(), 1..4),
            at in any::<prop::sample::Index>(),
            v in any::<u8>(),
        ) {
            let mut bytes = numbered(cs).to_envelope();
            let i = at.index(bytes.len());
            bytes[i] = v;
            if let Ok(set) = ConflictSet::decode(&bytes) {
                prop_assert_eq!(set.to_envelope(), bytes);
                prop_assert!(set.entries().iter().all(|(_, c)| c.validate().is_ok()));
            }
        }

        #[test]
        fn conflict_decode_survives_random_bytes(bytes in prop::collection::vec(any::<u8>(), 0..300)) {
            let mut with_header = vec![b'a', b's', 7, 1];
            with_header.extend(&bytes);
            let _ = ConflictSet::decode(&bytes);
            let _ = ConflictSet::decode(&with_header);
        }
    }
}
