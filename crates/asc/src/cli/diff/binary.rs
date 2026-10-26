//! `--binary`: git's `GIT binary patch` body (base85 lines of zlib-deflated literals) for a
//! change whose content is binary. Deltas are never emitted; a literal is always valid input
//! to `git apply`.

use std::io::{self, Write};

use asc_engine::diff::{ContentSource, TreeChange};
use bytes::Bytes;

/// git's base85 alphabet.
const ALPHABET: &[u8; 85] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~";

/// Raw bytes per output line.
const LINE: usize = 52;

/// The first line the engine prints for a binary change.
pub const NOTICE: &[u8] = b"Binary files ";

/// Encodes `data` as base85, four bytes to five characters, zero padded.
fn encode85(data: &[u8], out: &mut Vec<u8>) {
    for chunk in data.chunks(4) {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        let mut acc = u32::from_be_bytes(word);
        let mut digits = [0u8; 5];
        for d in digits.iter_mut().rev() {
            *d = ALPHABET[(acc % 85) as usize];
            acc /= 85;
        }
        out.extend_from_slice(&digits);
    }
}

/// git's `core.loosecompression`, else `core.compression`, else zlib's fastest level.
pub fn zlib_level(loose: Option<&str>, general: Option<&str>) -> i32 {
    let parse = |v: Option<&str>| {
        v.and_then(|v| v.trim().parse::<i32>().ok())
            .filter(|l| (-1..=9).contains(l))
    };
    match parse(loose).or_else(|| parse(general)) {
        Some(-1) => 6,
        Some(level) => level,
        None => 1,
    }
}

fn deflate(data: &[u8], level: i32) -> io::Result<Vec<u8>> {
    let level = gix::zlib::Compression::new(level).unwrap_or(gix::zlib::Compression::BEST_SPEED);
    let mut z = gix::zlib::stream::deflate::Write::new(Vec::new(), level);
    z.write_all(data)?;
    z.flush()?;
    Ok(z.into_inner())
}

/// One direction: `literal <size>` and the deflated content in length-prefixed base85 lines,
/// then a blank line.
fn body(w: &mut impl Write, content: &[u8], level: i32) -> io::Result<()> {
    let packed = deflate(content, level)?;
    writeln!(w, "literal {}", content.len())?;
    for line in packed.chunks(LINE) {
        let n = line.len();
        let prefix = if n <= 26 {
            b'A' + (n as u8 - 1)
        } else {
            b'a' + (n as u8 - 27)
        };
        let mut text = vec![prefix];
        encode85(line, &mut text);
        text.push(b'\n');
        w.write_all(&text)?;
    }
    w.write_all(b"\n")
}

async fn side(
    src: &dyn ContentSource,
    entry: &Option<asc_core::object::Entry>,
) -> asc_engine::diff::Result<Bytes> {
    match entry {
        Some(e) => src.read(e).await,
        None => Ok(Bytes::new()),
    }
}

/// git prints the full ids on the `index` line of a binary change, whatever `--abbrev` says.
fn full_index(patch: &mut Vec<u8>, c: &TreeChange) {
    let Some(at) = patch
        .windows(7)
        .position(|w| w == b"\nindex ")
        .map(|i| i + 1)
    else {
        return;
    };
    let end = patch[at..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(patch.len(), |i| at + i);
    let line = &patch[at + 6..end];
    let rest = line
        .iter()
        .position(|&b| b == b' ')
        .map_or(&[][..], |i| &line[i..])
        .to_vec();
    let id = |e: &Option<asc_core::object::Entry>| {
        e.as_ref()
            .map_or_else(|| "0".repeat(40), |e| e.git.to_string())
    };
    let mut text = format!("index {}..{}", id(&c.old), id(&c.new)).into_bytes();
    text.extend_from_slice(&rest);
    patch.splice(at..end, text);
}

/// Replaces the trailing `Binary files … differ` line of `patch` (one change's patch text) with
/// the binary patch body. A patch that does not end that way is text and stays as it is.
pub async fn upgrade(
    patch: &mut Vec<u8>,
    src: &dyn ContentSource,
    c: &TreeChange,
    level: i32,
) -> asc_engine::diff::Result<()> {
    let Some(without_eol) = patch.len().checked_sub(1) else {
        return Ok(());
    };
    let start = patch[..without_eol]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    let last = &patch[start..];
    if !last.starts_with(NOTICE) || !last.ends_with(b" differ\n") {
        return Ok(());
    }
    patch.truncate(start);
    full_index(patch, c);
    let (old, new) = (side(src, &c.old).await?, side(src, &c.new).await?);
    patch.extend_from_slice(b"GIT binary patch\n");
    body(patch, &new, level)?;
    body(patch, &old, level)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base85_matches_git() {
        // From `git diff --binary`: the 8 bytes of an empty level-1 zlib stream.
        let mut out = Vec::new();
        encode85(&[0x78, 0x01, 0x03, 0, 0, 0, 0, 1], &mut out);
        assert_eq!(out, b"cmV?d00001");
    }

    #[test]
    fn empty_literal() {
        let mut out = Vec::new();
        body(&mut out, b"", 1).unwrap();
        assert_eq!(out, b"literal 0\nHcmV?d00001\n\n");
    }

    #[test]
    fn compression_level_follows_git() {
        assert_eq!(zlib_level(None, None), 1);
        assert_eq!(zlib_level(None, Some("9")), 9);
        assert_eq!(zlib_level(Some("3"), Some("9")), 3);
        assert_eq!(zlib_level(None, Some("-1")), 6);
        assert_eq!(zlib_level(Some("12"), None), 1);
    }
}
