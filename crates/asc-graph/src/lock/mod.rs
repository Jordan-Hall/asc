//! Lockfile readers (SPEC §12A.4), one module per ecosystem.

pub mod bun;
pub mod cargo;

/// Most dependency edges one lockfile may resolve: a small multiple of
/// [`MAX_LOCKFILE_PACKAGES`](crate::model::MAX_LOCKFILE_PACKAGES). A lock over
/// it is unusable, so crafted names cannot amplify into quadratic work.
pub const MAX_LOCKFILE_EDGES: usize = 8 * crate::model::MAX_LOCKFILE_PACKAGES;
