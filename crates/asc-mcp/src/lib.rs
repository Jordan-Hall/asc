//! The MCP protocol layer of asc (SPEC 22): the tool catalogue, the schema generator and the
//! `tools/list` the server answers with. No repository logic lives here; backends plug in through
//! traits owned by later units (M05 and M08).
//!
//! - [`schema`]: JSON Schema 2020-12 for the asc JSON profile, generated from the proto descriptors.
//! - [`shape`]: the token budget, stream folding and structure-preserving truncation (SPEC 22.6.2).
//! - [`cursor`]: HMAC-sealed cursors and the key ring.
//! - [`handler`], [`backend`], [`config`], [`errors`]: the request handler over a backend trait, the
//!   `asc.mcp.*` trust rules and the error mapping (M05).
//! - [`resources`], [`prompts`]: the `asc://` resources, templates, completion and the five prompts (M07).
//! - [`catalogue`]: the tool table built from `tools.toml` by `build.rs`, gated by [`catalogue::Context`].

pub mod backend;
pub mod catalogue;
pub mod config;
pub mod confirm;
pub mod cursor;
pub mod errors;
pub mod handler;
pub mod prompts;
pub mod resources;
pub mod rpc_backend;
pub mod schema;
pub mod search;
pub mod shape;

pub use catalogue::ASC_HANDLES_SCHEMA;
