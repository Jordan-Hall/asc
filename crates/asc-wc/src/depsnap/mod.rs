//! Dependency snapshots (`docs/design/DEPENDENCY-SNAPSHOTS.md`): an installed `node_modules`
//! stored once as a tree under a key computed from the install's inputs.

pub mod key;

pub use key::{
    Ecosystem, EnvEntry, FileDigest, KeyError, KeyParts, Libc, Manager, PartDiff, Platform,
    Scripts, SnapshotKey,
};
