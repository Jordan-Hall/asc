//! `%(trailers…)`: the trailer block of a message and git's options for printing it.

/// One line group of the trailer block.
#[derive(Debug, PartialEq, Eq)]
pub enum Entry {
    /// `Key: value`, with the raw text including any continuation lines (no final newline).
    Trailer { key: Vec<u8>, value: Vec<u8>, raw: Vec<u8> },
    /// A line of the block that is not a trailer.
    Other(Vec<u8>),
}

/// The options of `%(trailers:…)`.
#[derive(Clone, Debug, Default)]
pub struct Opts {
    pub keys: Vec<Vec<u8>>,
    pub only: bool,
    pub unfold: bool,
    pub keyonly: bool,
    pub valueonly: bool,
    pub separator: Option<Vec<u8>>,
    pub kv_separator: Option<Vec<u8>>,
}

fn is_blank(line: &[u8]) -> bool {
    line.iter().all(u8::is_ascii_whitespace)
}

/// Position of the separator in `line` when it starts with a trailer token (git's
/// `find_separator`): alphanumerics and `-`, optionally followed by blanks, then `:`.
fn separator_pos(line: &[u8]) -> Option<usize> {
    let mut blank_seen = false;
    for (i, &c) in line.iter().enumerate() {
        if c == b':' {
            return Some(i);
        }
        if !blank_seen && (c.is_ascii_alphanumeric() || c == b'-') {
            continue;
        }
        if i != 0 && (c == b' ' || c == b'\t') {
            blank_seen = true;
            continue;
        }
        break;
    }
    None
}

/// The lines of `message`, each without its newline.
fn lines(message: &[u8]) -> Vec<&[u8]> {
    let mut v: Vec<&[u8]> = message.split(|&b| b == b'\n').collect();
    if v.last().is_some_and(|l| l.is_empty()) {
        v.pop();
    }
    v
}

/// Index of the first line of the trailer block, or `lines.len()` when there is none (git's
/// `find_trailer_block_start`).
fn block_start(lines: &[&[u8]]) -> usize {
    let Some(title_end) = lines.iter().position(|l| l.is_empty()) else {
        return lines.len();
    };
    let (mut trailer, mut other, mut cont) = (0usize, 0usize, 0usize);
    let (mut recognised, mut only_spaces) = (false, true);
    for i in (title_end..lines.len()).rev() {
        let line = lines[i];
        if is_blank(line) {
            if only_spaces {
                continue;
            }
            other += cont;
            if (recognised && trailer * 3 >= other) || (trailer > 0 && other == 0) {
                return i + 1;
            }
            return lines.len();
        }
        only_spaces = false;
        if line.starts_with(b"Signed-off-by: ") || line.starts_with(b"(cherry picked from commit ") {
            trailer += 1;
            cont = 0;
            recognised = true;
            continue;
        }
        if separator_pos(line).is_some_and(|p| p >= 1) && !line[0].is_ascii_whitespace() {
            trailer += 1;
            cont = 0;
        } else if line[0].is_ascii_whitespace() {
            cont += 1;
        } else {
            other += 1 + cont;
            cont = 0;
        }
    }
    lines.len()
}

/// The entries of the trailer block of `message`; empty when it has none.
pub fn parse(message: &[u8]) -> Vec<Entry> {
    let all = lines(message);
    let start = block_start(&all);
    let mut out: Vec<Entry> = Vec::new();
    let mut end = all.len();
    while end > start && is_blank(all[end - 1]) {
        end -= 1;
    }
    for line in &all[start.min(end)..end] {
        if line.first().is_some_and(u8::is_ascii_whitespace) {
            if let Some(Entry::Trailer { raw, value, .. }) = out.last_mut() {
                raw.push(b'\n');
                raw.extend_from_slice(line);
                value.push(b'\n');
                value.extend_from_slice(line);
            } else if let Some(Entry::Other(raw)) = out.last_mut() {
                raw.push(b'\n');
                raw.extend_from_slice(line);
            } else {
                out.push(Entry::Other(line.to_vec()));
            }
            continue;
        }
        match separator_pos(line) {
            Some(p) if p >= 1 => {
                let key = line[..p].trim_ascii_end().to_vec();
                let value = line[p + 1..].trim_ascii().to_vec();
                out.push(Entry::Trailer { key, value, raw: line.to_vec() });
            }
            _ => out.push(Entry::Other(line.to_vec())),
        }
    }
    out
}

/// `unfold`: continuation lines joined to the previous one with a single space.
fn unfolded(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut parts = text.split(|&b| b == b'\n');
    if let Some(first) = parts.next() {
        out.extend_from_slice(first);
    }
    for part in parts {
        out.push(b' ');
        out.extend_from_slice(part.trim_ascii_start());
    }
    out
}

fn key_matches(opts: &Opts, key: &[u8]) -> bool {
    opts.keys.is_empty()
        || opts.keys.iter().any(|k| {
            let k = k.strip_suffix(b":").unwrap_or(k);
            k.eq_ignore_ascii_case(key)
        })
}

/// Writes the entries as `%(trailers:…)` prints them.
pub fn render(entries: &[Entry], opts: &Opts, out: &mut Vec<u8>) {
    let mut first = true;
    for entry in entries {
        let text = match entry {
            Entry::Trailer { key, value, raw } => {
                if !key_matches(opts, key) {
                    continue;
                }
                let value = if opts.unfold { unfolded(value) } else { value.clone() };
                if opts.keyonly {
                    key.clone()
                } else if opts.valueonly {
                    value
                } else if let Some(sep) = &opts.kv_separator {
                    [key.as_slice(), sep, &value].concat()
                } else if opts.unfold {
                    unfolded(raw)
                } else {
                    raw.clone()
                }
            }
            Entry::Other(raw) => {
                if opts.only || !opts.keys.is_empty() {
                    continue;
                }
                if opts.unfold { unfolded(raw) } else { raw.clone() }
            }
        };
        match &opts.separator {
            Some(sep) => {
                if !first {
                    out.extend_from_slice(sep);
                }
                out.extend_from_slice(&text);
            }
            None => {
                out.extend_from_slice(&text);
                out.push(b'\n');
            }
        }
        first = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(message: &str, opts: &Opts) -> String {
        let mut out = Vec::new();
        render(&parse(message.as_bytes()), opts, &mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn block_after_the_last_blank_line() {
        let m = "subject\n\nbody\n\nSigned-off-by: A <a@x>\nReviewed-by: B\n";
        assert_eq!(shown(m, &Opts::default()), "Signed-off-by: A <a@x>\nReviewed-by: B\n");
    }

    #[test]
    fn prose_is_not_a_block() {
        assert_eq!(shown("subject\n\nsome: text\nand more\n", &Opts::default()), "");
        assert_eq!(shown("only: title\n", &Opts::default()), "");
    }

    #[test]
    fn key_filter_and_separator() {
        let m = "s\n\nA: 1\nB: 2\nA: 3\n";
        let opts = Opts { keys: vec![b"a".to_vec()], separator: Some(b", ".to_vec()), ..Opts::default() };
        assert_eq!(shown(m, &opts), "A: 1, A: 3");
        let opts = Opts { valueonly: true, ..Opts::default() };
        assert_eq!(shown(m, &opts), "1\n2\n3\n");
    }

    #[test]
    fn continuation_lines_unfold() {
        let m = "s\n\nA: one\n two\nB: x\n";
        assert_eq!(shown(m, &Opts::default()), "A: one\n two\nB: x\n");
        let opts = Opts { unfold: true, ..Opts::default() };
        assert_eq!(shown(m, &opts), "A: one two\nB: x\n");
    }
}
