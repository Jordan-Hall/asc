//! The read-only git view (docs/design/GIT-VIEW.md, roadmap row 26).
//!
//! The view lets real git and git libraries read an asc workspace through a projected, read-only
//! git directory whose loose objects are rendered on lookup from asc's state. This module holds the
//! pieces that need no I/O and no state; the resolver, the namespace and the daemon wiring build on
//! it.
//!
//! - [`object`]: the loose-object renderer. It frames an object body (a blob, a tree, a commit or a
//!   tag, produced by the same `asc_core::git` functions an export uses) as the bytes git stores
//!   under `objects/<2 hex>/<38 hex>`, with stored-deflate zlib so the framed size and any byte
//!   range are computed from the body length alone.

pub mod object;
