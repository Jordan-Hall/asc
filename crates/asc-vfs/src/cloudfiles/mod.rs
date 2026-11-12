//! Windows Cloud Files backend (SPEC §10.2b): lazy files through the Cloud Files API
//! (`cldapi.dll`, the filter OneDrive uses), for machines where ProjFS is not enabled. It needs no
//! optional Windows feature and no administrator rights.
//!
//! * The root is registered as a sync root of this workspace and connected while the daemon runs
//!   ([`provider`]). Directories populate on demand (`FETCH_PLACEHOLDERS`) and files hydrate on
//!   first read (`FETCH_DATA`) from the identity each placeholder carries, never from HEAD
//!   ([`callbacks`]).
//! * A placeholder's identity is `kind ‖ id ‖ size` for a file and `kind ‖ id` for a directory
//!   ([`Ident`]). A directory moved before it was listed still lists from its own tree.
//! * Cloud Files tells a provider about deletions and renames of placeholders only, so the
//!   journal is fed by a change watcher over the whole root ([`watch`]); the callbacks enforce the
//!   path rules (a locked path is never deleted or renamed).
//! * The switch, reconcile, dehydration and conversion back to a plain directory follow ProjFS's
//!   rules through the shared engine ([`switch`], [`walk`], [`unvirtualize`]).
//!
//! Every path the filter hands a callback is untrusted: it is converted once, validated as a
//! `RepoPath`, and resolved against the base tree. No lock is held across a Cloud Files call.

use std::panic::{AssertUnwindSafe, catch_unwind};

use asc_core::id::FileId;
use asc_core::object::Kind;

use crate::core::Node;

pub mod callbacks;
pub mod ffi;
pub mod populate;
pub mod provider;
pub mod relocate;
pub mod switch;
pub mod unvirtualize;
pub mod walk;
pub mod watch;

pub use ffi::ProviderUnavailable;
pub use provider::{CloudFilesProvider, Config};
pub use relocate::relocate;
pub use unvirtualize::unvirtualize;

pub use crate::win::{DbTimes, JournalSink, TimeSource, Unvirtualized};

/// Identity kinds (the same numbers as ProjFS's `ContentID`).
pub const CONTENT_FILE: u8 = 1;
pub const CONTENT_TREE: u8 = 3;
/// The identity of the sync root itself.
pub const ROOT_IDENTITY: &[u8] = b"asc-root";

/// What a placeholder's identity names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ident {
    /// A file, exec file or symlink: its content and size.
    File { id: FileId, size: u64 },
    /// A directory or gitlink: its tree (zero for a gitlink without a composed tree).
    Tree([u8; 32]),
    /// The sync root.
    Root,
}

impl Ident {
    /// The identity of a tree entry.
    pub fn of(node: &Node) -> Self {
        match node.kind {
            Kind::Dir | Kind::Gitlink => Self::Tree(node.id),
            _ => Self::File {
                id: FileId::from_bytes(node.id),
                size: node.size,
            },
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::File { id, size } => {
                let mut v = Vec::with_capacity(41);
                v.push(CONTENT_FILE);
                v.extend_from_slice(id.as_bytes());
                v.extend_from_slice(&size.to_le_bytes());
                v
            }
            Self::Tree(id) => {
                let mut v = Vec::with_capacity(33);
                v.push(CONTENT_TREE);
                v.extend_from_slice(id);
                v
            }
            Self::Root => ROOT_IDENTITY.to_vec(),
        }
    }

    /// `None` for bytes this provider never writes.
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b == ROOT_IDENTITY {
            return Some(Self::Root);
        }
        let (&kind, rest) = b.split_first()?;
        match (kind, rest.len()) {
            (CONTENT_FILE, 40) => {
                let id: [u8; 32] = rest[..32].try_into().ok()?;
                let size = u64::from_le_bytes(rest[32..].try_into().ok()?);
                Some(Self::File {
                    id: FileId::from_bytes(id),
                    size,
                })
            }
            (CONTENT_TREE, 32) => Some(Self::Tree(rest.try_into().ok()?)),
            _ => None,
        }
    }

    pub fn is_tree(&self) -> bool {
        !matches!(self, Self::File { .. })
    }
}

/// Runs a callback body; a panic must not unwind into the filter.
pub(crate) fn guard(f: impl FnOnce()) {
    let _ = catch_unwind(AssertUnwindSafe(f));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_round_trip_and_junk_is_refused() {
        let f = Ident::File {
            id: FileId::from_bytes([7; 32]),
            size: 1 << 40,
        };
        assert_eq!(Ident::decode(&f.encode()), Some(f));
        let t = Ident::Tree([9; 32]);
        assert_eq!(Ident::decode(&t.encode()), Some(t));
        assert_eq!(Ident::decode(ROOT_IDENTITY), Some(Ident::Root));
        assert!(t.is_tree() && Ident::Root.is_tree() && !f.is_tree());
        let mut junk = f.encode();
        junk.push(0);
        assert_eq!(Ident::decode(&junk), None);
        assert_eq!(Ident::decode(&[2; 33]), None);
        assert_eq!(Ident::decode(&[]), None);
    }

    #[test]
    fn guard_swallows_a_panic() {
        guard(|| panic!("boom"));
    }
}

#[cfg(test)]
mod lab;
