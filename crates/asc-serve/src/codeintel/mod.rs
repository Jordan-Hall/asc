//! Server-side code intelligence (SPEC §23.5). I13 owns this module file and `queue`; later
//! units add their own `pub mod` lines.

pub mod backfill;
pub mod files;
pub mod metrics;
pub mod modelgen;
pub mod ords;
pub mod queue;
pub mod rpc;
pub mod snaps;
pub mod sweep;
pub mod warm;
pub mod worker;
