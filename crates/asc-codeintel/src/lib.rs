//! Code intelligence (SPEC §23). I01 owns the per-content doc model: dense ordinals,
//! languages per path, the byte skip rules, `.asc/codeintel.toml` and the secret scanner.
//! Every function here treats its input as untrusted and bounds its work.

pub mod config;
pub mod doc;
pub mod grams;
pub mod lang;
pub mod plan;
pub mod secrets;
pub mod skip;
pub mod scope;
pub mod tags;
