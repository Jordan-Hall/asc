//! Object keys and bucket layout (SPEC §3.1, §3.3).

use std::collections::HashSet;
use std::fmt;
use std::sync::{Mutex, OnceLock};

use asc_core::id::{ChangesetId, ChunkId, FileId, GitOid, TagId, TreeId};
use data_encoding::HEXLOWER;

/// Address of one immutable object in a repository namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ObjKey {
    /// Inline file bytes (size <= 4 MiB).
    File(FileId),
    /// Chunk list envelope of a chunked file, stored under the file's id.
    ChunkList(FileId),
    /// One content-defined chunk.
    Chunk(ChunkId),
    Tree(TreeId),
    Changeset(ChangesetId),
    Tag(TagId),
    /// Raw git tree object (`tree <len>\0...`) that does not re-render canonically.
    GitTreeOverride(GitOid),
    /// Recomputable derived value.
    Derived {
        name: &'static str,
        version: u32,
        id: [u8; 32],
    },
}

/// Top-level key directory, one per [`ObjKey`] variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyClass {
    File,
    ChunkList,
    Chunk,
    Tree,
    Changeset,
    Tag,
    GitTreeOverride,
    Derived,
}

impl KeyClass {
    /// Directory name under the namespace root.
    pub const fn dir(self) -> &'static str {
        match self {
            Self::File => "f",
            Self::ChunkList => "l",
            Self::Chunk => "k",
            Self::Tree => "t",
            Self::Changeset => "c",
            Self::Tag => "g",
            Self::GitTreeOverride => "o",
            Self::Derived => "d",
        }
    }
}

/// A directory prefix for listing (GC only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyPrefix(Option<KeyClass>);

impl KeyPrefix {
    /// Every object key.
    pub const fn all() -> Self {
        Self(None)
    }
    /// Every key of one class.
    pub const fn class(class: KeyClass) -> Self {
        Self(Some(class))
    }
    /// Relative directory, or `None` for the whole namespace.
    pub fn dir(&self) -> Option<&'static str> {
        self.0.map(KeyClass::dir)
    }
}

fn leaf(dir: &str, hex: &str) -> String {
    format!("{dir}/{}/{hex}", &hex[..2])
}

/// Interns derived-value names so parsed keys can carry `&'static str`.
fn intern(name: &str) -> &'static str {
    static NAMES: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut set = NAMES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(s) = set.get(name) {
        return s;
    }
    let s: &'static str = Box::leak(name.into());
    set.insert(s);
    s
}

fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 || s.bytes().any(|c| c.is_ascii_uppercase()) {
        return None;
    }
    HEXLOWER.decode(s.as_bytes()).ok()?.try_into().ok()
}

impl ObjKey {
    /// Class (top-level directory) of this key.
    pub const fn class(&self) -> KeyClass {
        match self {
            Self::File(_) => KeyClass::File,
            Self::ChunkList(_) => KeyClass::ChunkList,
            Self::Chunk(_) => KeyClass::Chunk,
            Self::Tree(_) => KeyClass::Tree,
            Self::Changeset(_) => KeyClass::Changeset,
            Self::Tag(_) => KeyClass::Tag,
            Self::GitTreeOverride(_) => KeyClass::GitTreeOverride,
            Self::Derived { .. } => KeyClass::Derived,
        }
    }

    /// Layout key relative to the namespace root, e.g. `f/ab/abcd...`.
    pub fn path(&self) -> String {
        let dir = self.class().dir();
        match self {
            Self::File(i) | Self::ChunkList(i) => leaf(dir, &i.to_string()),
            Self::Chunk(i) => leaf(dir, &i.to_string()),
            Self::Tree(i) => leaf(dir, &i.to_string()),
            Self::Changeset(i) => leaf(dir, &i.to_string()),
            Self::Tag(i) => leaf(dir, &i.to_string()),
            Self::GitTreeOverride(i) => leaf(dir, &i.to_string()),
            Self::Derived { name, version, id } => {
                leaf(&format!("{dir}/{name}.v{version}"), &HEXLOWER.encode(id))
            }
        }
    }

    /// Inverse of [`path`](Self::path); `None` for anything that is not an object key.
    pub fn parse(path: &str) -> Option<Self> {
        let (dir, rest) = path.split_once('/')?;
        let sharded = |rest: &str| -> Option<String> {
            let (h2, hex) = rest.split_once('/')?;
            (hex.get(..2) == Some(h2)).then(|| hex.to_owned())
        };
        Some(match dir {
            "f" => Self::File(FileId::from_bytes(unhex(&sharded(rest)?)?)),
            "l" => Self::ChunkList(FileId::from_bytes(unhex(&sharded(rest)?)?)),
            "k" => Self::Chunk(ChunkId::from_bytes(unhex(&sharded(rest)?)?)),
            "t" => Self::Tree(TreeId::from_bytes(unhex(&sharded(rest)?)?)),
            "c" => Self::Changeset(ChangesetId::from_bytes(unhex(&sharded(rest)?)?)),
            "g" => Self::Tag(TagId::from_bytes(unhex(&sharded(rest)?)?)),
            "o" => Self::GitTreeOverride(GitOid::from_bytes(unhex(&sharded(rest)?)?)),
            "d" => {
                let (nv, rest) = rest.split_once('/')?;
                let (name, version) = nv.rsplit_once(".v")?;
                let version = version
                    .parse()
                    .ok()
                    .filter(|v: &u32| v.to_string() == version)?;
                Self::Derived {
                    name: intern(name),
                    version,
                    id: unhex(&sharded(rest)?)?,
                }
            }
            _ => return None,
        })
    }
}

impl fmt::Display for ObjKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples() -> Vec<ObjKey> {
        vec![
            ObjKey::File(FileId::of(b"a")),
            ObjKey::ChunkList(FileId::of(b"b")),
            ObjKey::Chunk(ChunkId::of(b"c")),
            ObjKey::Tree(TreeId::of(b"d")),
            ObjKey::Changeset(ChangesetId::of(b"e")),
            ObjKey::Tag(TagId::of(b"f")),
            ObjKey::GitTreeOverride(GitOid::from_bytes([0xab; 20])),
            ObjKey::Derived {
                name: "skip",
                version: 3,
                id: [0x5c; 32],
            },
        ]
    }

    #[test]
    fn layout_and_round_trip() {
        for k in samples() {
            assert_eq!(ObjKey::parse(&k.path()), Some(k), "{k}");
        }
        let id = FileId::from_bytes([0xab; 32]);
        let hex = "ab".repeat(32);
        assert_eq!(ObjKey::File(id).path(), format!("f/ab/{hex}"));
        assert_eq!(
            ObjKey::Derived {
                name: "gen",
                version: 1,
                id: [0xab; 32]
            }
            .path(),
            format!("d/gen.v1/ab/{hex}")
        );
    }

    #[test]
    fn parse_rejects_malformed() {
        let hex = "ab".repeat(32);
        for bad in [
            "",
            "f",
            "f/ab",
            &format!("f/cd/{hex}"),
            &format!("f/ab/{}", hex.to_uppercase()),
            &format!("z/ab/{hex}"),
            "rl/main/00000000000000000001",
            &format!("d/gen/ab/{hex}"),
            &format!("d/gen.v01/ab/{hex}"),
        ] {
            assert_eq!(ObjKey::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn classes_are_directories() {
        assert_eq!(KeyPrefix::all().dir(), None);
        assert_eq!(KeyPrefix::class(KeyClass::Tree).dir(), Some("t"));
        for k in samples() {
            assert!(k.path().starts_with(k.class().dir()));
        }
    }
}
