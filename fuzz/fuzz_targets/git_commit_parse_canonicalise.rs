#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| asc_fuzz::git_commit_parse_canonicalise(data));
