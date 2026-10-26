//! Conversions between `asc.v1` prost messages and `asc-core`/`asc-store` domain types (SPEC §7).
//!
//! Ids are checked by length and `IdMode`: 32 bytes is an asc id, 20 bytes a git oid. Every
//! conversion rejects wrong lengths, unknown enum values and invalid paths or signatures.

use asc_core::change::MetaId;
use asc_core::id::{ChangesetId, ChunkId, FileId, GitOid, RefTarget, TagId, TreeId};
use asc_core::path::{PathError, RepoPath};
use asc_core::sig::{SigError, Signature};
use asc_store::key::ObjKey;
use bytes::Bytes;

use crate::pb;

/// Why a wire message could not become a domain value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConvertError {
    /// An id field has the wrong number of bytes.
    #[error("{what}: expected {expected} bytes, got {got}")]
    BadLength {
        what: &'static str,
        expected: usize,
        got: usize,
    },
    /// An id has the length of the other id mode than the request declared.
    #[error("{what}: id length {got} does not match id mode {mode:?}")]
    ModeMismatch {
        what: &'static str,
        mode: IdMode,
        got: usize,
    },
    /// A required field or oneof arm is absent.
    #[error("missing field {0}")]
    Missing(&'static str),
    /// An enum field holds a value this version does not know or may not accept.
    #[error("{what}: unsupported value {value}")]
    BadEnum { what: &'static str, value: i32 },
    /// A number does not fit the domain type.
    #[error("{0} out of range")]
    OutOfRange(&'static str),
    /// A domain value has no wire form.
    #[error("{0} has no wire form")]
    Unsupported(&'static str),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Signature(#[from] SigError),
}

/// Result alias for this module.
pub type Result<T, E = ConvertError> = std::result::Result<T, E>;

/// Which id space a request speaks (`RepoRef.id_mode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum IdMode {
    /// BLAKE3 ids, 32 bytes.
    #[default]
    Asc,
    /// Git SHA-1 oids, 20 bytes, in and out.
    Git,
}

impl IdMode {
    /// Decodes the raw enum number of a message field.
    pub fn from_wire(v: i32) -> Result<Self> {
        match pb::IdMode::try_from(v) {
            Ok(pb::IdMode::Asc) => Ok(Self::Asc),
            Ok(pb::IdMode::Git) => Ok(Self::Git),
            Err(_) => Err(ConvertError::BadEnum {
                what: "IdMode",
                value: v,
            }),
        }
    }

    /// The raw enum number for a message field.
    pub fn to_wire(self) -> i32 {
        match self {
            Self::Asc => pb::IdMode::Asc as i32,
            Self::Git => pb::IdMode::Git as i32,
        }
    }
}

/// A fixed-length id that travels as raw bytes.
pub trait WireId: Sized + Copy {
    /// Byte length on the wire.
    const LEN: usize;
    /// Type name for error messages.
    const NAME: &'static str;
    /// Decodes exactly [`Self::LEN`] bytes.
    fn from_slice(b: &[u8]) -> Result<Self>;
    /// The raw bytes.
    fn as_slice(&self) -> &[u8];
}

macro_rules! wire_id {
    ($t:ty, $n:literal) => {
        impl WireId for $t {
            const LEN: usize = $n;
            const NAME: &'static str = stringify!($t);
            fn from_slice(b: &[u8]) -> Result<Self> {
                let a: [u8; $n] = b.try_into().map_err(|_| ConvertError::BadLength {
                    what: Self::NAME,
                    expected: $n,
                    got: b.len(),
                })?;
                Ok(<$t>::from_bytes(a))
            }
            fn as_slice(&self) -> &[u8] {
                self.as_bytes()
            }
        }
    };
}

wire_id!(FileId, 32);
wire_id!(ChunkId, 32);
wire_id!(TreeId, 32);
wire_id!(ChangesetId, 32);
wire_id!(TagId, 32);
wire_id!(GitOid, 20);
wire_id!(MetaId, 32);

/// Decodes an optional id: empty bytes mean absent.
pub fn opt_id<T: WireId>(b: &[u8]) -> Result<Option<T>> {
    if b.is_empty() {
        Ok(None)
    } else {
        T::from_slice(b).map(Some)
    }
}

/// Encodes an optional id; absent becomes empty bytes.
pub fn opt_id_to_wire<T: WireId>(id: Option<&T>) -> Bytes {
    id.map_or_else(Bytes::new, |i| Bytes::copy_from_slice(i.as_slice()))
}

/// An id that is an asc id in [`IdMode::Asc`] and a git oid in [`IdMode::Git`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModeId<T> {
    Asc(T),
    Git(GitOid),
}

impl<T: WireId> ModeId<T> {
    /// Decodes by length alone: `T::LEN` bytes is an asc id, 20 bytes a git oid.
    pub fn from_wire(b: &[u8]) -> Result<Self> {
        if b.len() == T::LEN {
            T::from_slice(b).map(Self::Asc)
        } else if b.len() == GitOid::LEN {
            GitOid::from_slice(b).map(Self::Git)
        } else {
            Err(ConvertError::BadLength {
                what: T::NAME,
                expected: T::LEN,
                got: b.len(),
            })
        }
    }

    /// Decodes under a declared mode; a length from the other mode is rejected.
    pub fn from_wire_in(b: &[u8], mode: IdMode) -> Result<Self> {
        let id = Self::from_wire(b).map_err(|_| ConvertError::BadLength {
            what: T::NAME,
            expected: if mode == IdMode::Asc {
                T::LEN
            } else {
                GitOid::LEN
            },
            got: b.len(),
        })?;
        if id.mode() == mode {
            Ok(id)
        } else {
            Err(ConvertError::ModeMismatch {
                what: T::NAME,
                mode,
                got: b.len(),
            })
        }
    }

    /// Decodes an optional id (empty means absent) under a declared mode.
    pub fn opt_from_wire_in(b: &[u8], mode: IdMode) -> Result<Option<Self>> {
        if b.is_empty() {
            Ok(None)
        } else {
            Self::from_wire_in(b, mode).map(Some)
        }
    }

    /// The id space this value belongs to.
    pub fn mode(&self) -> IdMode {
        match self {
            Self::Asc(_) => IdMode::Asc,
            Self::Git(_) => IdMode::Git,
        }
    }

    /// The raw bytes.
    pub fn to_wire(&self) -> Bytes {
        match self {
            Self::Asc(i) => Bytes::copy_from_slice(i.as_slice()),
            Self::Git(g) => Bytes::copy_from_slice(g.as_slice()),
        }
    }
}

fn opt_mode_id_to_wire<T: WireId>(id: Option<&ModeId<T>>) -> Bytes {
    id.map_or_else(Bytes::new, ModeId::to_wire)
}

// ---------------------------------------------------------------- ObjKey

impl TryFrom<&pb::ObjKey> for ObjKey {
    type Error = ConvertError;

    /// Kind 0 (unspecified) is rejected; 9 is `ChangeMeta` (SPEC §20.2).
    fn try_from(k: &pb::ObjKey) -> Result<Self> {
        let kind = pb::ObjKind::try_from(k.kind).map_err(|_| ConvertError::BadEnum {
            what: "ObjKind",
            value: k.kind,
        })?;
        Ok(match kind {
            pb::ObjKind::File => Self::File(FileId::from_slice(&k.id)?),
            pb::ObjKind::ChunkList => Self::ChunkList(FileId::from_slice(&k.id)?),
            pb::ObjKind::Chunk => Self::Chunk(ChunkId::from_slice(&k.id)?),
            pb::ObjKind::Tree => Self::Tree(TreeId::from_slice(&k.id)?),
            pb::ObjKind::Changeset => Self::Changeset(ChangesetId::from_slice(&k.id)?),
            pb::ObjKind::Tag => Self::Tag(TagId::from_slice(&k.id)?),
            pb::ObjKind::GitTreeOverride => Self::GitTreeOverride(GitOid::from_slice(&k.id)?),
            pb::ObjKind::ChangeMeta => Self::ChangeMeta(MetaId::from_slice(&k.id)?),
            pb::ObjKind::Unspecified => {
                return Err(ConvertError::BadEnum {
                    what: "ObjKind",
                    value: k.kind,
                });
            }
        })
    }
}

impl TryFrom<&ObjKey> for pb::ObjKey {
    type Error = ConvertError;

    /// Derived values are local caches and never travel.
    fn try_from(k: &ObjKey) -> Result<Self> {
        let (kind, id): (pb::ObjKind, &[u8]) = match k {
            ObjKey::File(i) => (pb::ObjKind::File, i.as_slice()),
            ObjKey::ChunkList(i) => (pb::ObjKind::ChunkList, i.as_slice()),
            ObjKey::Chunk(i) => (pb::ObjKind::Chunk, i.as_slice()),
            ObjKey::Tree(i) => (pb::ObjKind::Tree, i.as_slice()),
            ObjKey::Changeset(i) => (pb::ObjKind::Changeset, i.as_slice()),
            ObjKey::Tag(i) => (pb::ObjKind::Tag, i.as_slice()),
            ObjKey::GitTreeOverride(i) => (pb::ObjKind::GitTreeOverride, i.as_slice()),
            ObjKey::ChangeMeta(i) => (pb::ObjKind::ChangeMeta, i.as_slice()),
            ObjKey::Derived { .. } => return Err(ConvertError::Unsupported("derived ObjKey")),
        };
        Ok(Self {
            kind: kind as i32,
            id: Bytes::copy_from_slice(id),
        })
    }
}

// ------------------------------------------------------------- RefTarget

impl TryFrom<&pb::RefTarget> for RefTarget {
    type Error = ConvertError;

    fn try_from(t: &pb::RefTarget) -> Result<Self> {
        match t.t.as_ref().ok_or(ConvertError::Missing("RefTarget.t"))? {
            pb::ref_target::T::Changeset(b) => Ok(Self::Changeset(ChangesetId::from_slice(b)?)),
            pb::ref_target::T::Tag(b) => Ok(Self::Tag(TagId::from_slice(b)?)),
        }
    }
}

impl From<&RefTarget> for pb::RefTarget {
    fn from(t: &RefTarget) -> Self {
        let arm = match t {
            RefTarget::Changeset(c) => {
                pb::ref_target::T::Changeset(Bytes::copy_from_slice(c.as_slice()))
            }
            RefTarget::Tag(g) => pb::ref_target::T::Tag(Bytes::copy_from_slice(g.as_slice())),
        };
        Self { t: Some(arm) }
    }
}

// ------------------------------------------------------------- Signature

impl TryFrom<&pb::Signature> for Signature {
    type Error = ConvertError;

    fn try_from(s: &pb::Signature) -> Result<Self> {
        let tz = i16::try_from(s.tz_min).map_err(|_| ConvertError::OutOfRange("tz_min"))?;
        Ok(Self::new(s.name.to_vec(), s.email.to_vec(), s.time, tz)?)
    }
}

impl From<&Signature> for pb::Signature {
    fn from(s: &Signature) -> Self {
        Self {
            name: Bytes::copy_from_slice(s.name()),
            email: Bytes::copy_from_slice(s.email()),
            time: s.time(),
            tz_min: i32::from(s.tz_min()),
        }
    }
}

// ------------------------------------------------------------ PathChange

/// One changed path of a diff or changeset (`asc.v1.PathChange`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathChange {
    pub path: RepoPath,
    /// Status letter(s) as in `git diff --name-status` (`A`, `M`, `D`, `R`, `C`, `T`).
    pub status: String,
    pub old_path: Option<RepoPath>,
    pub old_mode: u32,
    pub new_mode: u32,
    pub old_git: Option<GitOid>,
    pub new_git: Option<GitOid>,
    pub old_id: Option<FileId>,
    pub new_id: Option<FileId>,
    pub old_size: u64,
    pub new_size: u64,
    /// Rename or copy similarity in percent.
    pub similarity: u32,
}

fn opt_path(b: &[u8]) -> Result<Option<RepoPath>> {
    if b.is_empty() {
        Ok(None)
    } else {
        Ok(Some(RepoPath::new(b)?))
    }
}

impl TryFrom<&pb::PathChange> for PathChange {
    type Error = ConvertError;

    fn try_from(c: &pb::PathChange) -> Result<Self> {
        Ok(Self {
            path: RepoPath::new(&c.path[..])?,
            status: c.status.clone(),
            old_path: opt_path(&c.old_path)?,
            old_mode: c.old_mode,
            new_mode: c.new_mode,
            old_git: opt_id(&c.old_git)?,
            new_git: opt_id(&c.new_git)?,
            old_id: opt_id(&c.old_id)?,
            new_id: opt_id(&c.new_id)?,
            old_size: c.old_size,
            new_size: c.new_size,
            similarity: c.similarity,
        })
    }
}

impl From<&PathChange> for pb::PathChange {
    fn from(c: &PathChange) -> Self {
        Self {
            path: Bytes::copy_from_slice(c.path.as_bytes()),
            status: c.status.clone(),
            old_path: c
                .old_path
                .as_ref()
                .map_or_else(Bytes::new, |p| Bytes::copy_from_slice(p.as_bytes())),
            old_mode: c.old_mode,
            new_mode: c.new_mode,
            old_git: opt_id_to_wire(c.old_git.as_ref()),
            new_git: opt_id_to_wire(c.new_git.as_ref()),
            old_id: opt_id_to_wire(c.old_id.as_ref()),
            new_id: opt_id_to_wire(c.new_id.as_ref()),
            old_size: c.old_size,
            new_size: c.new_size,
            similarity: c.similarity,
        }
    }
}

// -------------------------------------------------------------- Conflict

/// Why a path conflicts (`asc.v1.ConflictKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConflictKind {
    Content,
    AddAdd,
    ModifyDelete,
    FileDir,
    CaseFold,
    Binary,
}

impl ConflictKind {
    fn from_wire(v: i32) -> Result<Self> {
        Ok(match pb::ConflictKind::try_from(v) {
            Ok(pb::ConflictKind::Content) => Self::Content,
            Ok(pb::ConflictKind::AddAdd) => Self::AddAdd,
            Ok(pb::ConflictKind::ModifyDelete) => Self::ModifyDelete,
            Ok(pb::ConflictKind::FileDir) => Self::FileDir,
            Ok(pb::ConflictKind::CaseFold) => Self::CaseFold,
            Ok(pb::ConflictKind::Binary) => Self::Binary,
            Err(_) => {
                return Err(ConvertError::BadEnum {
                    what: "ConflictKind",
                    value: v,
                });
            }
        })
    }

    fn to_wire(self) -> i32 {
        (match self {
            Self::Content => pb::ConflictKind::Content,
            Self::AddAdd => pb::ConflictKind::AddAdd,
            Self::ModifyDelete => pb::ConflictKind::ModifyDelete,
            Self::FileDir => pb::ConflictKind::FileDir,
            Self::CaseFold => pb::ConflictKind::CaseFold,
            Self::Binary => pb::ConflictKind::Binary,
        }) as i32
    }
}

/// A path that could not be merged (`asc.v1.Conflict`). Ids are in the request's [`IdMode`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub changeset: ModeId<ChangesetId>,
    pub path: RepoPath,
    pub kind: ConflictKind,
    pub base: Option<ModeId<FileId>>,
    pub ours: Option<ModeId<FileId>>,
    pub theirs: Option<ModeId<FileId>>,
    /// The diff3-style marked result (SPEC §8.11).
    pub marked: Option<ModeId<FileId>>,
}

impl TryFrom<&pb::Conflict> for Conflict {
    type Error = ConvertError;

    /// Ids may be of either mode; each field is decoded by length.
    fn try_from(c: &pb::Conflict) -> Result<Self> {
        let opt = |b: &[u8]| -> Result<Option<ModeId<FileId>>> {
            if b.is_empty() {
                Ok(None)
            } else {
                ModeId::from_wire(b).map(Some)
            }
        };
        Ok(Self {
            changeset: ModeId::from_wire(&c.changeset)?,
            path: RepoPath::new(&c.path[..])?,
            kind: ConflictKind::from_wire(c.kind)?,
            base: opt(&c.base_id)?,
            ours: opt(&c.ours_id)?,
            theirs: opt(&c.theirs_id)?,
            marked: opt(&c.marked_id)?,
        })
    }
}

impl From<&Conflict> for pb::Conflict {
    fn from(c: &Conflict) -> Self {
        Self {
            changeset: c.changeset.to_wire(),
            path: Bytes::copy_from_slice(c.path.as_bytes()),
            kind: c.kind.to_wire(),
            base_id: opt_mode_id_to_wire(c.base.as_ref()),
            ours_id: opt_mode_id_to_wire(c.ours.as_ref()),
            theirs_id: opt_mode_id_to_wire(c.theirs.as_ref()),
            marked_id: opt_mode_id_to_wire(c.marked.as_ref()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cs(n: u8) -> ChangesetId {
        ChangesetId::from_bytes([n; 32])
    }
    fn fid(n: u8) -> FileId {
        FileId::from_bytes([n; 32])
    }
    fn git(n: u8) -> GitOid {
        GitOid::from_bytes([n; 20])
    }

    #[test]
    fn id_round_trip_and_bad_lengths() {
        assert_eq!(FileId::from_slice(fid(1).as_slice()), Ok(fid(1)));
        assert_eq!(GitOid::from_slice(git(2).as_slice()), Ok(git(2)));
        for len in [0usize, 19, 21, 31, 33] {
            let b = vec![0u8; len];
            assert!(matches!(
                TreeId::from_slice(&b),
                Err(ConvertError::BadLength { expected: 32, got, .. }) if got == len
            ));
        }
        assert!(GitOid::from_slice(&[0; 32]).is_err());
        assert_eq!(opt_id::<FileId>(&[]), Ok(None));
        assert!(opt_id::<FileId>(&[1; 5]).is_err());
    }

    #[test]
    fn mode_id_by_length_and_mode() {
        let c = ModeId::Asc(cs(3));
        let g = ModeId::<ChangesetId>::Git(git(4));
        assert_eq!(ModeId::from_wire(&c.to_wire()), Ok(c));
        assert_eq!(ModeId::from_wire(&g.to_wire()), Ok(g));
        assert!(ModeId::<ChangesetId>::from_wire(&[0; 25]).is_err());
        assert_eq!(ModeId::from_wire_in(&c.to_wire(), IdMode::Asc), Ok(c));
        assert_eq!(ModeId::from_wire_in(&g.to_wire(), IdMode::Git), Ok(g));
        assert!(ModeId::<ChangesetId>::from_wire_in(&g.to_wire(), IdMode::Asc).is_err());
        assert!(matches!(
            ModeId::<ChangesetId>::from_wire_in(&c.to_wire(), IdMode::Git),
            Err(ConvertError::BadLength { expected: 20, .. } | ConvertError::ModeMismatch { .. })
        ));
        assert_eq!(
            ModeId::<FileId>::opt_from_wire_in(&[], IdMode::Git),
            Ok(None)
        );
    }

    #[test]
    fn id_mode_round_trip() {
        for m in [IdMode::Asc, IdMode::Git] {
            assert_eq!(IdMode::from_wire(m.to_wire()), Ok(m));
        }
        assert!(IdMode::from_wire(9).is_err());
    }

    #[test]
    fn obj_key_round_trip_every_kind() {
        let keys = [
            ObjKey::File(fid(1)),
            ObjKey::ChunkList(fid(2)),
            ObjKey::Chunk(ChunkId::from_bytes([3; 32])),
            ObjKey::Tree(TreeId::from_bytes([4; 32])),
            ObjKey::Changeset(cs(5)),
            ObjKey::Tag(TagId::from_bytes([6; 32])),
            ObjKey::GitTreeOverride(git(7)),
        ];
        for k in keys {
            let w = pb::ObjKey::try_from(&k).unwrap();
            assert_eq!(ObjKey::try_from(&w), Ok(k));
        }
    }

    #[test]
    fn obj_key_rejects_bad_input() {
        let mk = |kind: i32, len: usize| pb::ObjKey {
            kind,
            id: Bytes::from(vec![0u8; len]),
        };
        // Wrong length for the kind, in both directions.
        assert!(ObjKey::try_from(&mk(pb::ObjKind::File as i32, 20)).is_err());
        assert!(ObjKey::try_from(&mk(pb::ObjKind::GitTreeOverride as i32, 32)).is_err());
        // Unspecified, removed (8) and unknown kinds.
        for kind in [0, 8, 10, -1] {
            assert!(ObjKey::try_from(&mk(kind, 32)).is_err(), "kind {kind}");
        }
        assert_eq!(pb::ObjKind::ChangeMeta as i32, 9);
        let derived = ObjKey::Derived {
            name: "skip",
            version: 1,
            id: [0; 32],
        };
        assert!(pb::ObjKey::try_from(&derived).is_err());
    }

    #[test]
    fn ref_target_round_trip() {
        for t in [
            RefTarget::Changeset(cs(1)),
            RefTarget::Tag(TagId::from_bytes([2; 32])),
        ] {
            let w = pb::RefTarget::from(&t);
            assert_eq!(RefTarget::try_from(&w), Ok(t));
        }
        assert_eq!(
            RefTarget::try_from(&pb::RefTarget { t: None }),
            Err(ConvertError::Missing("RefTarget.t"))
        );
        let short = pb::RefTarget {
            t: Some(pb::ref_target::T::Tag(Bytes::from_static(&[1; 20]))),
        };
        assert!(RefTarget::try_from(&short).is_err());
    }

    #[test]
    fn signature_round_trip_and_rejects() {
        let s = Signature::new(
            b"Ada".to_vec(),
            b"ada@example.com".to_vec(),
            1_700_000_000,
            -330,
        )
        .unwrap();
        let w = pb::Signature::from(&s);
        assert_eq!(Signature::try_from(&w), Ok(s));
        let bad_name = pb::Signature {
            name: Bytes::from_static(b"a<b"),
            ..w.clone()
        };
        assert_eq!(
            Signature::try_from(&bad_name),
            Err(ConvertError::Signature(SigError::Name))
        );
        let bad_email = pb::Signature {
            email: Bytes::from_static(b"a\nb"),
            ..w.clone()
        };
        assert!(Signature::try_from(&bad_email).is_err());
        let bad_tz = pb::Signature {
            tz_min: 70_000,
            ..w
        };
        assert_eq!(
            Signature::try_from(&bad_tz),
            Err(ConvertError::OutOfRange("tz_min"))
        );
    }

    fn sample_change() -> PathChange {
        PathChange {
            path: RepoPath::new("src/new.rs").unwrap(),
            status: "R".into(),
            old_path: Some(RepoPath::new("src/old.rs").unwrap()),
            old_mode: 0o100644,
            new_mode: 0o100755,
            old_git: Some(git(1)),
            new_git: Some(git(2)),
            old_id: Some(fid(3)),
            new_id: None,
            old_size: 10,
            new_size: 12,
            similarity: 90,
        }
    }

    #[test]
    fn path_change_round_trip() {
        let c = sample_change();
        let w = pb::PathChange::from(&c);
        assert_eq!(PathChange::try_from(&w), Ok(c));
        let minimal = PathChange {
            old_path: None,
            old_git: None,
            new_git: None,
            old_id: None,
            ..sample_change()
        };
        assert_eq!(
            PathChange::try_from(&pb::PathChange::from(&minimal)),
            Ok(minimal)
        );
    }

    #[test]
    fn path_change_rejects_bad_input() {
        let ok = pb::PathChange::from(&sample_change());
        let bad_path = pb::PathChange {
            path: Bytes::from_static(b"a//b"),
            ..ok.clone()
        };
        assert!(PathChange::try_from(&bad_path).is_err());
        let empty_path = pb::PathChange {
            path: Bytes::new(),
            ..ok.clone()
        };
        assert!(PathChange::try_from(&empty_path).is_err());
        let bad_git = pb::PathChange {
            old_git: Bytes::from_static(&[1; 32]),
            ..ok.clone()
        };
        assert!(PathChange::try_from(&bad_git).is_err());
        let bad_id = pb::PathChange {
            new_id: Bytes::from_static(&[1; 7]),
            ..ok
        };
        assert!(PathChange::try_from(&bad_id).is_err());
    }

    #[test]
    fn conflict_round_trip_both_modes() {
        let asc = Conflict {
            changeset: ModeId::Asc(cs(1)),
            path: RepoPath::new("Cargo.lock").unwrap(),
            kind: ConflictKind::Content,
            base: Some(ModeId::Asc(fid(2))),
            ours: Some(ModeId::Asc(fid(3))),
            theirs: None,
            marked: Some(ModeId::Asc(fid(4))),
        };
        let g = Conflict {
            changeset: ModeId::Git(git(1)),
            kind: ConflictKind::ModifyDelete,
            base: Some(ModeId::Git(git(2))),
            ours: None,
            theirs: Some(ModeId::Git(git(3))),
            marked: None,
            ..asc.clone()
        };
        for c in [asc, g] {
            let w = pb::Conflict::from(&c);
            assert_eq!(Conflict::try_from(&w), Ok(c));
        }
        for kind in [
            ConflictKind::AddAdd,
            ConflictKind::FileDir,
            ConflictKind::CaseFold,
            ConflictKind::Binary,
        ] {
            assert_eq!(ConflictKind::from_wire(kind.to_wire()), Ok(kind));
        }
    }

    #[test]
    fn conflict_rejects_bad_input() {
        let ok = pb::Conflict::from(&Conflict {
            changeset: ModeId::Asc(cs(1)),
            path: RepoPath::new("a").unwrap(),
            kind: ConflictKind::Binary,
            base: None,
            ours: None,
            theirs: None,
            marked: None,
        });
        assert!(
            Conflict::try_from(&pb::Conflict {
                changeset: Bytes::from_static(&[1; 31]),
                ..ok.clone()
            })
            .is_err()
        );
        assert!(
            Conflict::try_from(&pb::Conflict {
                kind: 99,
                ..ok.clone()
            })
            .is_err()
        );
        assert!(
            Conflict::try_from(&pb::Conflict {
                ours_id: Bytes::from_static(&[1; 3]),
                ..ok
            })
            .is_err()
        );
    }
}
