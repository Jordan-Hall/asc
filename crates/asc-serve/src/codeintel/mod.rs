//! Server-side code intelligence (SPEC §23.5). I13 owns this module file and `queue`; later
//! units add their own `pub mod` lines.

pub mod backfill;
pub mod queue;
pub mod modelgen;
pub mod sweep;
pub mod warm;
pub mod worker;
