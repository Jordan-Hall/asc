//! `grep -n` text: `path:line:text` (SPEC §22.3), with an optional `rev:` prefix as `git grep <rev>`.

use super::{clip_line, quote_path};

/// One matching line.
#[derive(Clone, Debug)]
pub struct Hit<'a> {
    pub path: &'a [u8],
    /// One-based line number.
    pub lineno: u32,
    pub text: &'a [u8],
}

/// Renders `hits`. With `rev`, each line starts `rev:` as `git grep <rev>` prints. Paths are
/// quoted as git quotes them (`core.quotePath` is `quote_high`), so one hit is always one line.
#[must_use]
pub fn render(hits: &[Hit<'_>], rev: Option<&str>, quote_high: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for h in hits {
        if let Some(r) = rev {
            out.extend_from_slice(r.as_bytes());
            out.push(b':');
        }
        out.extend_from_slice(&quote_path(h.path, false, quote_high));
        out.extend_from_slice(format!(":{}:", h.lineno).as_bytes());
        out.extend_from_slice(&clip_line(h.text));
        out.push(b'\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        let hits = [
            Hit {
                path: b"src/a.rs",
                lineno: 3,
                text: b"fn main() {}\n",
            },
            Hit {
                path: b"we ird\n",
                lineno: 12,
                text: b"x:y",
            },
        ];
        assert_eq!(
            render(&hits, None, true),
            b"src/a.rs:3:fn main() {}\n\"we ird\\n\":12:x:y\n".to_vec()
        );
        assert_eq!(
            render(&hits[..1], Some("HEAD"), true),
            b"HEAD:src/a.rs:3:fn main() {}\n"
        );
    }

    #[test]
    fn long_lines_are_bounded() {
        let big = vec![b'z'; 100_000];
        let out = render(
            &[Hit {
                path: b"f",
                lineno: 1,
                text: &big,
            }],
            None,
            true,
        );
        assert!(out.len() < 9000);
    }
}
