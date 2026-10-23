//! `--shell`, `--perl`, `--python` and `--tcl`: how an atom's value is quoted as a string
//! literal of the host language (git's `quote.c`).

/// The quoting a `--shell`-style flag selects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Quote {
    #[default]
    None,
    Shell,
    Perl,
    Python,
    Tcl,
}

impl Quote {
    /// The style a flag spells (`--shell`), if it is one.
    pub fn from_flag(flag: &str) -> Option<Self> {
        Some(match flag {
            "--shell" | "-s" => Self::Shell,
            "--perl" | "-p" => Self::Perl,
            "--python" => Self::Python,
            "--tcl" => Self::Tcl,
            _ => return None,
        })
    }

    /// Appends `value` quoted in this style.
    pub fn apply(self, value: &[u8], out: &mut Vec<u8>) {
        match self {
            Self::None => out.extend_from_slice(value),
            Self::Shell => {
                out.push(b'\'');
                for &b in value {
                    match b {
                        b'\'' => out.extend_from_slice(b"'\\''"),
                        b'!' => out.extend_from_slice(b"'\\!'"),
                        _ => out.push(b),
                    }
                }
                out.push(b'\'');
            }
            Self::Perl => {
                out.push(b'\'');
                for &b in value {
                    if matches!(b, b'\'' | b'\\') {
                        out.push(b'\\');
                    }
                    out.push(b);
                }
                out.push(b'\'');
            }
            Self::Python => {
                out.push(b'\'');
                for &b in value {
                    match b {
                        b'\n' => out.extend_from_slice(b"\\n"),
                        b'\'' | b'\\' => out.extend_from_slice(&[b'\\', b]),
                        _ => out.push(b),
                    }
                }
                out.push(b'\'');
            }
            Self::Tcl => {
                out.push(b'"');
                for &b in value {
                    match b {
                        b'[' | b']' | b'{' | b'}' | b'$' | b'\\' | b'"' => {
                            out.extend_from_slice(&[b'\\', b])
                        }
                        0x0c => out.extend_from_slice(b"\\f"),
                        b'\r' => out.extend_from_slice(b"\\r"),
                        b'\n' => out.extend_from_slice(b"\\n"),
                        b'\t' => out.extend_from_slice(b"\\t"),
                        0x0b => out.extend_from_slice(b"\\v"),
                        _ => out.push(b),
                    }
                }
                out.push(b'"');
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quoted(style: Quote, text: &str) -> String {
        let mut out = Vec::new();
        style.apply(text.as_bytes(), &mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn each_language_escapes_its_own_specials() {
        assert_eq!(quoted(Quote::Shell, "it's!"), "'it'\\''s'\\!''");
        assert_eq!(quoted(Quote::Perl, "a'b\\c"), "'a\\'b\\\\c'");
        assert_eq!(quoted(Quote::Python, "a\nb'c"), "'a\\nb\\'c'");
        assert_eq!(quoted(Quote::Tcl, "[x]\t$"), "\"\\[x\\]\\t\\$\"");
        assert_eq!(quoted(Quote::None, "raw'"), "raw'");
        assert_eq!(quoted(Quote::Shell, ""), "''");
    }
}
