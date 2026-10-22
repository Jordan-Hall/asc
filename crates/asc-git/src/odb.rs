//! Git object database access (SPEC §9.4, §11): loose objects, packs and alternates.
//!
//! Used only by import/export and git-remote mode. Reads re-hash, so a corrupt
//! object never reaches the caller as valid content.

use std::path::Path;

use bytes::Bytes;
use asc_core::git::{ObjectKind, blob_oid, object_oid};
use asc_core::id::GitOid;
use gix::objs::{Exists, Find, Write};

/// Why an object could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum OdbError {
    #[error("cannot open object database at {path}: {source}")]
    Open {
        path: String,
        source: std::io::Error,
    },
    #[error("object {0} not found")]
    NotFound(GitOid),
    #[error("object {oid} is a {found}, expected a blob")]
    NotBlob { oid: GitOid, found: &'static str },
    #[error("object {expected} is corrupt: content hashes to {actual}")]
    Corrupt { expected: GitOid, actual: GitOid },
    #[error("reading object {oid}: {message}")]
    Read { oid: GitOid, message: String },
    #[error("writing object: {0}")]
    Write(String),
}

/// Converts to gix's object id.
pub(crate) fn to_gix(oid: GitOid) -> gix::hash::ObjectId {
    gix::hash::ObjectId::from_bytes_or_panic(oid.as_bytes())
}

/// Converts from gix's object id; `None` unless it is a SHA-1.
pub(crate) fn from_gix(oid: &gix::hash::oid) -> Option<GitOid> {
    <[u8; 20]>::try_from(oid.as_bytes())
        .ok()
        .map(GitOid::from_bytes)
}

fn kind_name(kind: gix::objs::Kind) -> &'static str {
    match kind {
        gix::objs::Kind::Blob => "blob",
        gix::objs::Kind::Tree => "tree",
        gix::objs::Kind::Commit => "commit",
        gix::objs::Kind::Tag => "tag",
    }
}

fn to_gix_kind(kind: ObjectKind) -> gix::objs::Kind {
    match kind {
        ObjectKind::Blob => gix::objs::Kind::Blob,
        ObjectKind::Tree => gix::objs::Kind::Tree,
        ObjectKind::Commit => gix::objs::Kind::Commit,
        ObjectKind::Tag => gix::objs::Kind::Tag,
    }
}

/// A repository's object database. Cheap handles are not `Sync`; open one per thread.
pub struct Odb {
    handle: gix::odb::Handle,
}

impl Odb {
    /// Opens `<common_dir>/objects`, following its alternates.
    pub fn open(common_dir: &Path) -> Result<Self, OdbError> {
        let dir = common_dir.join("objects");
        let handle =
            gix::odb::at(&dir, gix::hash::Kind::Sha1).map_err(|source| OdbError::Open {
                path: dir.display().to_string(),
                source,
            })?;
        Ok(Self { handle })
    }

    /// Whether the object exists (loose, packed or in an alternate).
    pub fn contains(&self, oid: GitOid) -> bool {
        self.handle.exists(&to_gix(oid))
    }

    /// Reads a blob and checks that its content hashes to `oid`.
    pub fn read_blob_verified(&self, oid: GitOid) -> Result<Bytes, OdbError> {
        let mut buf = Vec::new();
        let data = self
            .handle
            .try_find(&to_gix(oid), &mut buf)
            .map_err(|e| OdbError::Read {
                oid,
                message: e.to_string(),
            })?
            .ok_or(OdbError::NotFound(oid))?;
        if data.kind != gix::objs::Kind::Blob {
            return Err(OdbError::NotBlob {
                oid,
                found: kind_name(data.kind),
            });
        }
        let actual = blob_oid(data.data);
        if actual != oid {
            return Err(OdbError::Corrupt {
                expected: oid,
                actual,
            });
        }
        Ok(Bytes::copy_from_slice(data.data))
    }

    /// An independent handle onto the same database, for another thread.
    pub fn fork(&self) -> Self {
        Self {
            handle: self.handle.clone(),
        }
    }

    /// Reads any object and checks that its content hashes to `oid`.
    pub fn read_object_verified(&self, oid: GitOid) -> Result<(ObjectKind, Vec<u8>), OdbError> {
        let mut buf = Vec::new();
        let data = self
            .handle
            .try_find(&to_gix(oid), &mut buf)
            .map_err(|e| OdbError::Read {
                oid,
                message: e.to_string(),
            })?
            .ok_or(OdbError::NotFound(oid))?;
        let kind = match data.kind {
            gix::objs::Kind::Blob => ObjectKind::Blob,
            gix::objs::Kind::Tree => ObjectKind::Tree,
            gix::objs::Kind::Commit => ObjectKind::Commit,
            gix::objs::Kind::Tag => ObjectKind::Tag,
        };
        let actual = object_oid(kind, data.data);
        if actual != oid {
            return Err(OdbError::Corrupt {
                expected: oid,
                actual,
            });
        }
        let body = data.data.to_vec();
        Ok((kind, body))
    }

    /// Writes a loose object and returns its id.
    pub fn write_loose(&self, kind: ObjectKind, bytes: &[u8]) -> Result<GitOid, OdbError> {
        let id = self
            .handle
            .write_buf(to_gix_kind(kind), bytes)
            .map_err(|e| OdbError::Write(e.to_string()))?;
        let oid = from_gix(&id).ok_or_else(|| OdbError::Write("non-SHA-1 object id".into()))?;
        debug_assert_eq!(oid, object_oid(kind, bytes));
        Ok(oid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git") // lint:allow-raw-git
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_owned()
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-q"]);
        dir
    }

    #[test]
    fn write_then_git_reads_it() {
        let dir = repo();
        let odb = Odb::open(&dir.path().join(".git")).expect("open");
        let oid = odb
            .write_loose(ObjectKind::Blob, b"hello\n")
            .expect("write");
        assert_eq!(oid, blob_oid(b"hello\n"));
        assert!(odb.contains(oid));
        assert_eq!(
            git(dir.path(), &["cat-file", "-p", &oid.to_string()]),
            "hello"
        );
        assert_eq!(
            odb.read_blob_verified(oid).expect("read"),
            Bytes::from_static(b"hello\n")
        );
    }

    #[test]
    fn reads_git_written_and_packed_objects() {
        let dir = repo();
        std::fs::write(dir.path().join("f"), b"packed content\n").expect("write");
        let id = git(dir.path(), &["hash-object", "-w", "f"]);
        git(dir.path(), &["repack", "-adq"]);
        git(dir.path(), &["prune-packed"]);
        let odb = Odb::open(&dir.path().join(".git")).expect("open");
        let oid: GitOid = id.parse().expect("oid");
        assert!(odb.contains(oid));
        assert_eq!(
            odb.read_blob_verified(oid).expect("read"),
            Bytes::from_static(b"packed content\n")
        );
    }

    #[test]
    fn missing_and_wrong_kind_and_corrupt() {
        let dir = repo();
        let odb = Odb::open(&dir.path().join(".git")).expect("open");
        let missing = blob_oid(b"nope");
        assert!(!odb.contains(missing));
        assert!(matches!(
            odb.read_blob_verified(missing),
            Err(OdbError::NotFound(_))
        ));
        let tree = odb.write_loose(ObjectKind::Tree, b"").expect("tree");
        assert!(matches!(
            odb.read_blob_verified(tree),
            Err(OdbError::NotBlob { found: "tree", .. })
        ));
        // A loose file holding a different object's content under this name.
        let wrong = blob_oid(b"claimed");
        let real = odb.write_loose(ObjectKind::Blob, b"actual").expect("blob");
        let hex = wrong.to_string();
        let target = dir.path().join(".git/objects").join(&hex[..2]);
        std::fs::create_dir_all(&target).expect("mkdir");
        let src = dir
            .path()
            .join(".git/objects")
            .join(&real.to_string()[..2])
            .join(&real.to_string()[2..]);
        std::fs::copy(src, target.join(&hex[2..])).expect("copy");
        let fresh = Odb::open(&dir.path().join(".git")).expect("open");
        assert!(matches!(
            fresh.read_blob_verified(wrong),
            Err(OdbError::Corrupt { .. } | OdbError::Read { .. })
        ));
    }
}
