//! U25a acceptance: the tool trace corpus (SPEC §17.1). The suite lives in
//! `tests/traces/main.rs`, next to the traces; this file only builds it as the `traces` test of
//! the `asc` package, so it can reach the built `asc` binary and the compat runner.

#[path = "../../../tests/traces/main.rs"]
mod suite;
