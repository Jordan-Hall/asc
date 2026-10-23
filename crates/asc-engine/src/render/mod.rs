//! Text renderers shared by `asc` and `asc-mcp` (SPEC §22.7.2, unit M04a).
//!
//! Each renderer is a pure function from plain data to bytes, so the CLI and the MCP server
//! print the same text and cannot drift. Nothing here reads a repository.

pub mod blame;
pub mod grep;
pub mod log;
pub mod status;

/// git's `quote_path`: C-style quoting when a byte needs it. `quote_space` also quotes a space
/// and `quote_high` quotes bytes of 0x80 and above (`core.quotePath`).
#[must_use]
pub fn quote_path(path: &[u8], quote_space: bool, quote_high: bool) -> Vec<u8> {
    let needs = |b: u8| b < 0x20 || b == 0x7f || b == b'"' || b == b'\\' || (b >= 0x80 && quote_high);
    if !path.iter().any(|&b| needs(b) || (quote_space && b == b' ')) {
        return path.to_vec();
    }
    let mut out = Vec::with_capacity(path.len() + 2);
    out.push(b'"');
    for &b in path {
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
            b if needs(b) => out.extend_from_slice(format!("\\{b:03o}").as_bytes()),
            b => out.push(b),
        }
    }
    out.push(b'"');
    out
}

/// `path` relative to the directory `prefix` (`""` or `a/b/`), with `../` where needed.
#[must_use]
pub fn relativize(path: &[u8], prefix: &[u8]) -> Vec<u8> {
    if prefix.is_empty() {
        return path.to_vec();
    }
    let dirs: Vec<&[u8]> = prefix.split(|&b| b == b'/').filter(|p| !p.is_empty()).collect();
    let is_dir = path.ends_with(b"/");
    let comps: Vec<&[u8]> = path.split(|&b| b == b'/').filter(|p| !p.is_empty()).collect();
    let usable = if is_dir { comps.len() } else { comps.len().saturating_sub(1) };
    let common = dirs.iter().zip(&comps).take(usable).take_while(|(a, b)| a == b).count();
    let mut out = b"../".repeat(dirs.len() - common);
    out.extend_from_slice(&comps[common..].join(&b'/'));
    if is_dir && common < comps.len() {
        out.push(b'/');
    }
    if out.is_empty() {
        out.extend_from_slice(b"./");
    }
    out
}

/// Longest text of one source line any renderer prints; longer lines are cut and end in `...`.
pub const MAX_LINE_BYTES: usize = 8192;

/// `line` cut to [`MAX_LINE_BYTES`] (marked with `...`), without its trailing newline.
pub(crate) fn clip_line(line: &[u8]) -> Vec<u8> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.len() <= MAX_LINE_BYTES {
        return line.to_vec();
    }
    let mut out = line[..MAX_LINE_BYTES].to_vec();
    out.extend_from_slice(b"...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(quote_path(b"plain", true, true), b"plain");
        assert_eq!(quote_path(b"a b", true, true), b"\"a b\"");
        assert_eq!(quote_path(b"a b", false, true), b"a b");
        assert_eq!(quote_path("é".as_bytes(), false, true), b"\"\\303\\251\"");
        assert_eq!(quote_path("é".as_bytes(), false, false), "é".as_bytes());
        assert_eq!(quote_path(b"t\tq\"b\\", false, true), b"\"t\\tq\\\"b\\\\\"");
        assert_eq!(quote_path(b"n\nl", false, true), b"\"n\\nl\"");
    }

    #[test]
    fn relative_paths() {
        assert_eq!(relativize(b"sub/x", b"sub/"), b"x");
        assert_eq!(relativize(b"a.txt", b"sub/"), b"../a.txt");
        assert_eq!(relativize(b"sub/deep/x", b"sub/other/"), b"../deep/x");
        assert_eq!(relativize(b"x", b""), b"x");
        assert_eq!(relativize(b"sub/", b"sub/"), b"./");
        assert_eq!(relativize(b"sub/new/", b"sub/"), b"new/");
    }

    #[test]
    fn clipping() {
        assert_eq!(clip_line(b"abc\r\n"), b"abc");
        let long = vec![b'x'; MAX_LINE_BYTES + 10];
        let out = clip_line(&long);
        assert_eq!(out.len(), MAX_LINE_BYTES + 3);
        assert!(out.ends_with(b"..."));
    }
}
