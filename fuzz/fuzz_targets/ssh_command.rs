//! Fuzz target for the SSH listener's command parser (SPEC §25.11.4; PLAN U86).
//!
//! `parse_command` must be total, and whatever it accepts must be a plain `owner/name` that no
//! shell, path or argv rule could misread.
#![no_main]

use asc_serve::ssh::policy::{MAX_COMMAND_LINE, parse_command};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(command) = parse_command(data) else {
        return;
    };
    assert!(data.len() <= MAX_COMMAND_LINE);
    let mut parts = command.repo.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        panic!("not owner/name: {:?}", command.repo);
    };
    for segment in [owner, name] {
        assert!(!segment.is_empty() && segment.len() <= 100);
        assert!(!segment.starts_with('.') && !segment.ends_with('.'));
        assert!(
            segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        );
    }
    // The accepted bytes are one verb, one space and one quoted word of plain bytes.
    assert!(data.iter().all(|&b| (0x20..0x7f).contains(&b)));
});
