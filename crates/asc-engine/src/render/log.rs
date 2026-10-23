//! `log --oneline` text: `<abbrev> [(decorations) ]<subject>` per commit.

use std::io::Write;

use asc_core::id::GitOid;

use super::clip_line;

/// Longest abbreviation printed (a full SHA-1).
const MAX_ABBREV: usize = 40;
/// Shortest abbreviation git prints.
const MIN_ABBREV: usize = 4;

/// One commit as `--oneline` shows it.
#[derive(Clone, Debug)]
pub struct Entry<'a> {
    pub id: GitOid,
    /// The subject line (see [`subject`]).
    pub subject: &'a [u8],
    /// Ref names for `--decorate`, already ordered; empty for none.
    pub decorations: &'a [String],
}

/// The subject of `message`: its first paragraph joined into one line, as git's `%s`.
#[must_use]
pub fn subject(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in message.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.iter().all(u8::is_ascii_whitespace) {
            if out.is_empty() {
                continue;
            }
            break;
        }
        if !out.is_empty() {
            out.push(b' ');
        }
        out.extend_from_slice(line.trim_ascii());
    }
    out
}

/// Renders `entries` one per line. `abbrev` is clamped to 4..=40 hex digits.
#[must_use]
pub fn oneline(entries: &[Entry<'_>], abbrev: usize) -> Vec<u8> {
    let n = abbrev.clamp(MIN_ABBREV, MAX_ABBREV);
    let mut out = Vec::new();
    for e in entries {
        let hex = e.id.to_string();
        out.extend_from_slice(&hex.as_bytes()[..n.min(hex.len())]);
        if !e.decorations.is_empty() {
            let _ = write!(out, " ({})", e.decorations.join(", "));
        }
        if !e.subject.is_empty() {
            out.push(b' ');
            out.extend_from_slice(&clip_line(e.subject));
        }
        out.push(b'\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_joins_first_paragraph() {
        assert_eq!(subject(b"one\ntwo\n\nbody\n"), b"one two");
        assert_eq!(subject(b"\n\n  lead  \r\nnext"), b"lead next");
        assert_eq!(subject(b""), b"");
    }

    #[test]
    fn lines_with_decorations() {
        let id = GitOid::from_bytes([0xab; 20]);
        let dec = vec!["HEAD -> master".to_owned(), "tag: v1".to_owned()];
        let entries = [
            Entry { id, subject: b"first", decorations: &dec },
            Entry { id, subject: b"", decorations: &[] },
        ];
        let text = String::from_utf8(oneline(&entries, 7)).unwrap();
        assert_eq!(text, "abababa (HEAD -> master, tag: v1) first\nabababa\n");
    }

    #[test]
    fn abbrev_is_clamped() {
        let id = GitOid::from_bytes([0x12; 20]);
        let e = [Entry { id, subject: b"s", decorations: &[] }];
        assert_eq!(oneline(&e, 0), b"1212 s\n");
        assert_eq!(oneline(&e, 999).len(), 40 + 3);
    }
}
