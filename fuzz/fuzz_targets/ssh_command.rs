//! Fuzz target for the SSH listener's command parser (SPEC §25.11.4; PLAN U86). The body is
//! `asc_fuzz::ssh_command`, so `cargo test` replays `regressions/ssh_command/` through it.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| asc_fuzz::ssh_command(data));
