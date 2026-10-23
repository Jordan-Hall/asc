//! `blame` text in git's default layout: `<abbrev> (<author> <date> <lineno>) <text>`.

use super::clip_line;

/// Most author characters of the name column.
const MAX_AUTHOR: usize = 64;

/// One blamed line.
#[derive(Clone, Debug)]
pub struct Line<'a> {
    /// Abbreviated origin id (hex), already cut to its display width.
    pub abbrev: &'a str,
    pub author: &'a str,
    /// The date as the caller formats it.
    pub date: &'a str,
    /// One-based line number.
    pub lineno: u32,
    pub text: &'a [u8],
    /// The line comes from a boundary commit (`^` prefix).
    pub boundary: bool,
}

/// Renders `lines`; the author column is padded to the longest name (at most 64 characters).
#[must_use]
pub fn render(lines: &[Line<'_>]) -> Vec<u8> {
    let cut = |a: &str| a.chars().take(MAX_AUTHOR).collect::<String>();
    let width = lines
        .iter()
        .map(|l| cut(l.author).chars().count())
        .max()
        .unwrap_or(0);
    let nwidth = lines
        .iter()
        .map(|l| l.lineno.to_string().len())
        .max()
        .unwrap_or(1);
    let mut out = Vec::new();
    for l in lines {
        if l.boundary {
            out.push(b'^');
        }
        out.extend_from_slice(l.abbrev.as_bytes());
        let author = cut(l.author);
        let pad = " ".repeat(width - author.chars().count());
        out.extend_from_slice(
            format!(" ({author}{pad} {} {:>nwidth$}) ", l.date, l.lineno).as_bytes(),
        );
        out.extend_from_slice(&clip_line(l.text));
        out.push(b'\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line<'a>(author: &'a str, n: u32, text: &'a [u8], boundary: bool) -> Line<'a> {
        Line {
            abbrev: "abc1234",
            author,
            date: "2026-01-02",
            lineno: n,
            text,
            boundary,
        }
    }

    #[test]
    fn pads_columns() {
        let lines = [
            line("Al", 9, b"x", false),
            line("Barbara", 10, b"y\r\n", true),
        ];
        let text = String::from_utf8(render(&lines)).unwrap();
        assert_eq!(
            text,
            "abc1234 (Al      2026-01-02  9) x\n^abc1234 (Barbara 2026-01-02 10) y\n"
        );
    }

    #[test]
    fn empty_input() {
        assert!(render(&[]).is_empty());
    }

    #[test]
    fn long_author_is_cut() {
        let long = "a".repeat(200);
        let text = render(&[line(&long, 1, b"", false)]);
        let s = String::from_utf8(text).unwrap();
        assert!(s.contains(&format!("({} ", "a".repeat(64))));
        assert!(!s.contains(&"a".repeat(65)));
    }
}
