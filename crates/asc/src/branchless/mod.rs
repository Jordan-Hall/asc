//! Branchless mode (SPEC §21): the client switches and their effective-mode resolver (B01).
//!
//! Later branchless units add their own `pub mod` line here (PLAN, branchless track).

pub mod heads_sink;
pub mod local_gc;
pub mod migrate;
pub mod mode;
pub mod pull;
pub mod push_map;
