//! Path output as git prints it: C-style quoting (`core.quotePath`) unless the line ends in NUL,
//! and names relative to the directory the command runs in.

use std::io::{self, Write};

/// git's `quote_c_style`: the quoted form of `name`, or `None` when it prints as is.
pub fn c_quote(name: &[u8], quote_high: bool) -> Option<Vec<u8>> {
    let special = |b: u8| b < 0x20 || b == 0x7f || b == b'"' || b == b'\\' || (quote_high && b >= 0x80);
    if !name.iter().any(|&b| special(b)) {
        return None;
    }
    let mut out = vec![b'"'];
    for &b in name {
        match b {
            7 => out.extend_from_slice(b"\\a"),
            8 => out.extend_from_slice(b"\\b"),
            9 => out.extend_from_slice(b"\\t"),
            10 => out.extend_from_slice(b"\\n"),
            11 => out.extend_from_slice(b"\\v"),
            12 => out.extend_from_slice(b"\\f"),
            13 => out.extend_from_slice(b"\\r"),
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b if special(b) => out.extend_from_slice(format!("\\{b:03o}").as_bytes()),
            b => out.push(b),
        }
    }
    out.push(b'"');
    Some(out)
}

/// How names print: quoted, unless lines end in NUL.
#[derive(Clone, Copy, Debug)]
pub struct Names {
    pub terminator: u8,
    pub quote_high: bool,
}

impl Names {
    /// `write_name_quoted`: the name and the terminator.
    pub fn write(&self, out: &mut dyn Write, name: &[u8]) -> io::Result<()> {
        self.write_bare(out, name)?;
        out.write_all(&[self.terminator])
    }

    /// The name alone, quoted unless NUL-terminated.
    pub fn write_bare(&self, out: &mut dyn Write, name: &[u8]) -> io::Result<()> {
        match (self.terminator, c_quote(name, self.quote_high)) {
            (0, _) | (_, None) => out.write_all(name),
            (_, Some(quoted)) => out.write_all(&quoted),
        }
    }
}

/// `relative_path`: `name` below `prefix` (which ends in `/` or is empty) without it, else with
/// `../` steps as git's `relative_path` makes them.
pub fn relative(name: &[u8], prefix: &[u8]) -> Vec<u8> {
    if prefix.is_empty() {
        return name.to_vec();
    }
    let mut common = 0;
    let mut i = 0;
    while i < name.len() && i < prefix.len() && name[i] == prefix[i] {
        if name[i] == b'/' {
            common = i + 1;
        }
        i += 1;
    }
    let ups = prefix[common..].iter().filter(|&&b| b == b'/').count();
    let mut out = b"../".repeat(ups);
    out.extend_from_slice(&name[common..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_rules() {
        assert_eq!(c_quote(b"plain name", true), None);
        assert_eq!(c_quote(b"a\"b", true).unwrap(), b"\"a\\\"b\"");
        assert_eq!(c_quote("é".as_bytes(), true).unwrap(), b"\"\\303\\251\"");
        assert_eq!(c_quote("é".as_bytes(), false), None);
        assert_eq!(c_quote(b"t\tab\n", true).unwrap(), b"\"t\\tab\\n\"");
    }

    #[test]
    fn relative_names() {
        assert_eq!(relative(b"sub/a.txt", b"sub/"), b"a.txt");
        assert_eq!(relative(b"a.txt", b"sub/"), b"../a.txt");
        assert_eq!(relative(b"sub/x/y", b"sub/"), b"x/y");
        assert_eq!(relative(b"b/c", b"a/d/"), b"../../b/c");
        assert_eq!(relative(b"a.txt", b""), b"a.txt");
    }
}
