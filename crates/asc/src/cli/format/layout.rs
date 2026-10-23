//! Display-width layout for `%<(`, `%>(`, `%w(`: git's `utf8_strnwidth`, `strbuf_utf8_replace`
//! and `strbuf_add_wrapped_text`.

/// Length of an ANSI `ESC [ … m` sequence at the start of `s`, else 0.
fn esc_len(s: &[u8]) -> usize {
    if s.len() < 3 || s[0] != 0x1b || s[1] != b'[' {
        return 0;
    }
    let mut i = 2;
    while i < s.len() && (s[i].is_ascii_digit() || s[i] == b';') {
        i += 1;
    }
    if s.get(i) == Some(&b'm') { i + 1 } else { 0 }
}

/// Terminal columns of a scalar value: 0 for combining marks, 2 for wide and fullwidth forms.
fn char_width(c: char) -> usize {
    let u = c as u32;
    if u == 0
        || (0x300..=0x36f).contains(&u)
        || (0x200b..=0x200f).contains(&u)
        || (0xfe00..=0xfe0f).contains(&u)
        || (0x1ab0..=0x1aff).contains(&u)
        || (0x20d0..=0x20ff).contains(&u)
    {
        return 0;
    }
    let wide = (0x1100..=0x115f).contains(&u)
        || (0x2e80..=0xa4cf).contains(&u) && u != 0x303f
        || (0xac00..=0xd7a3).contains(&u)
        || (0xf900..=0xfaff).contains(&u)
        || (0xfe30..=0xfe6f).contains(&u)
        || (0xff00..=0xff60).contains(&u)
        || (0xffe0..=0xffe6).contains(&u)
        || (0x1f300..=0x1f64f).contains(&u)
        || (0x1f900..=0x1f9ff).contains(&u)
        || (0x20000..=0x3fffd).contains(&u);
    if wide { 2 } else { 1 }
}

/// One step through `s`: the byte length of the next unit and its column width. Escape
/// sequences have width 0; a byte that is not UTF-8 counts as one column.
fn step(s: &[u8]) -> (usize, usize) {
    let e = esc_len(s);
    if e > 0 {
        return (e, 0);
    }
    let need = match s[0] {
        0..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return (1, 1),
    };
    match s.get(..need).and_then(|b| std::str::from_utf8(b).ok()) {
        Some(t) => (need, t.chars().next().map_or(0, char_width)),
        None => (1, 1),
    }
}

/// Columns `s` occupies, not counting colour escapes.
pub fn width(s: &[u8]) -> usize {
    let mut i = 0;
    let mut w = 0;
    while i < s.len() {
        let (n, cols) = step(&s[i..]);
        i += n;
        w += cols;
    }
    w
}

/// `strbuf_utf8_replace`: removes `len` columns starting at column `pos` and puts `with` there.
/// Colour escapes inside the removed span are kept.
pub fn replace_columns(s: &[u8], pos: usize, len: usize, with: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let (mut i, mut col) = (0, 0);
    let mut inserted = false;
    while i < s.len() {
        let (n, cols) = step(&s[i..]);
        let unit = &s[i..i + n];
        let is_esc = esc_len(&s[i..]) > 0;
        if col >= pos && !inserted {
            out.extend_from_slice(with);
            inserted = true;
        }
        if is_esc || col < pos || col >= pos + len {
            out.extend_from_slice(unit);
        }
        col += cols;
        i += n;
    }
    if !inserted {
        out.extend_from_slice(with);
    }
    out
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `strbuf_add_indented_text`: indents the first line by `indent1` and the others by `indent2`.
fn add_indented(buf: &mut Vec<u8>, text: &[u8], mut indent: usize, indent2: usize) {
    let mut rest = text;
    while !rest.is_empty() {
        let end = rest
            .iter()
            .position(|&b| b == b'\n')
            .map_or(rest.len(), |p| p + 1);
        buf.extend(std::iter::repeat_n(b' ', indent));
        buf.extend_from_slice(&rest[..end]);
        rest = &rest[end..];
        indent = indent2;
    }
}

/// `strbuf_add_wrapped_text`: appends `text` wrapped at `width` columns, the first line indented
/// by `indent1` and the rest by `indent2`.
pub fn add_wrapped(buf: &mut Vec<u8>, text: &[u8], indent1: usize, indent2: usize, width: usize) {
    if width == 0 {
        add_indented(buf, text, indent1, indent2);
        return;
    }
    let at = |i: usize| text.get(i).copied().unwrap_or(0);
    let mut bol = 0;
    let (mut w, mut indent) = (indent1, indent1);
    let mut space: Option<usize> = None;
    let mut i = 0;
    loop {
        let e = esc_len(&text[i.min(text.len())..]);
        if e > 0 {
            i += e;
            continue;
        }
        let c = at(i);
        if c == 0 || is_space(c) {
            if w <= width || space.is_none() {
                let mut start = bol;
                if c == 0 && i == start {
                    return;
                }
                match space {
                    Some(s) => start = s,
                    None => buf.extend(std::iter::repeat_n(b' ', indent)),
                }
                buf.extend_from_slice(&text[start..i]);
                if c == 0 {
                    return;
                }
                let mut s = i;
                let mut new_line = false;
                if c == b'\t' {
                    w |= 0x07;
                } else if c == b'\n' {
                    s += 1;
                    if at(s) == b'\n' {
                        buf.push(b'\n');
                        new_line = true;
                    } else if !at(s).is_ascii_alphanumeric() {
                        new_line = true;
                    } else {
                        buf.push(b' ');
                    }
                }
                if new_line {
                    wrap_line(
                        buf,
                        text,
                        &mut bol,
                        &mut i,
                        &mut space,
                        s,
                        &mut w,
                        &mut indent,
                        indent2,
                    );
                    continue;
                }
                space = Some(s);
                w += 1;
                i += 1;
            } else {
                let s = space.unwrap_or(i);
                wrap_line(
                    buf,
                    text,
                    &mut bol,
                    &mut i,
                    &mut space,
                    s,
                    &mut w,
                    &mut indent,
                    indent2,
                );
            }
            continue;
        }
        let (n, cols) = step(&text[i..]);
        w += cols;
        i += n;
    }
}

/// The `new_line:` arm of the wrap loop.
#[expect(clippy::too_many_arguments)]
fn wrap_line(
    buf: &mut Vec<u8>,
    text: &[u8],
    bol: &mut usize,
    i: &mut usize,
    space: &mut Option<usize>,
    s: usize,
    w: &mut usize,
    indent: &mut usize,
    indent2: usize,
) {
    buf.push(b'\n');
    let skip = usize::from(text.get(s).is_some_and(|&c| is_space(c)));
    *i = s + skip;
    *bol = *i;
    *space = None;
    *w = indent2;
    *indent = indent2;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrapped(text: &str, i1: usize, i2: usize, w: usize) -> String {
        let mut b = Vec::new();
        add_wrapped(&mut b, text.as_bytes(), i1, i2, w);
        String::from_utf8(b).unwrap()
    }

    #[test]
    fn widths() {
        assert_eq!(width(b"abc"), 3);
        assert_eq!(width("日本".as_bytes()), 4);
        assert_eq!(width(b"\x1b[31mred\x1b[m"), 3);
        assert_eq!(width("e\u{301}".as_bytes()), 1);
    }

    #[test]
    fn replace_keeps_ends() {
        assert_eq!(replace_columns(b"abcdefghij", 3, 7, b".."), b"abc..");
        assert_eq!(replace_columns(b"abcdefghij", 0, 7, b".."), b"..hij");
        assert_eq!(replace_columns(b"abcdefghij", 1, 7, b".."), b"a..ij");
    }

    #[test]
    fn wraps_at_word_boundaries() {
        assert_eq!(wrapped("aaa bbb ccc", 0, 0, 7), "aaa bbb\nccc");
        assert_eq!(wrapped("aaa bbb ccc", 2, 4, 9), "  aaa bbb\n    ccc");
        assert_eq!(wrapped("abcdefghij\n]", 2, 4, 10), "  abcdefghij\n    ]");
    }
}
