//! git's line-ending conversion (`convert.c`): what is stored when a work tree file is read
//! (`crlf_to_git`, see [`Eol`]) and what a checkout writes back (`crlf_to_worktree`, see
//! [`Smudge`]). The per-path decision comes from the `text`, `crlf` and `eol` attributes and from
//! `core.autocrlf` and `core.eol`; it is made by whoever knows them (`asc add`'s attributes).

use std::borrow::Cow;

use crate::status::Eol;

/// Bytes checked for NUL by the ingest normalisation's binary test.
const BINARY_PROBE: usize = 8000;

/// What a checkout does to a file's line endings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Smudge {
    /// The stored bytes are written as they are.
    #[default]
    None,
    /// `text` (or `eol=crlf`) with CRLF output: every lone LF becomes CRLF.
    Crlf,
    /// `text=auto` (or `core.autocrlf=true`) with CRLF output: as `Crlf`, unless the content is
    /// binary or already holds a CR.
    AutoCrlf,
}

/// Line-ending counts of some content, as git's `gather_stats` makes them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub nul: u64,
    pub lone_cr: u64,
    pub lone_lf: u64,
    pub crlf: u64,
    pub printable: u64,
    pub nonprintable: u64,
    /// The last byte fed was a CR whose pair is not known yet.
    pending_cr: bool,
    /// The last byte fed was `^Z`, which git does not count at the end of a file.
    last_eof: bool,
}

impl Stats {
    pub fn of(data: &[u8]) -> Self {
        let mut s = Self::default();
        s.feed(data);
        s.finish()
    }

    /// Counts the next piece of the content.
    pub fn feed(&mut self, data: &[u8]) {
        for &b in data {
            if std::mem::take(&mut self.pending_cr) {
                if b == b'\n' {
                    self.crlf += 1;
                    self.last_eof = false;
                    continue;
                }
                self.lone_cr += 1;
            }
            self.last_eof = b == 0x1a;
            match b {
                b'\r' => self.pending_cr = true,
                b'\n' => self.lone_lf += 1,
                0x7f => self.nonprintable += 1,
                0x08 | b'\t' | 0x1b | 0x0c => self.printable += 1,
                0 => {
                    self.nul += 1;
                    self.nonprintable += 1;
                }
                1..=31 => self.nonprintable += 1,
                _ => self.printable += 1,
            }
        }
    }

    /// The counts once all the content was fed.
    #[must_use]
    pub fn finish(mut self) -> Self {
        if std::mem::take(&mut self.pending_cr) {
            self.lone_cr += 1;
            self.last_eof = false;
        }
        if std::mem::take(&mut self.last_eof) {
            self.nonprintable = self.nonprintable.saturating_sub(1);
        }
        self
    }

    /// git's `convert_is_binary`.
    pub fn is_binary(&self) -> bool {
        self.lone_cr > 0 || self.nul > 0 || (self.printable >> 7) < self.nonprintable
    }

    /// git's `has_crlf_in_index` on content: text with CRLFs.
    pub fn text_with_crlf(&self) -> bool {
        !self.is_binary() && self.crlf > 0
    }

    /// git's `will_convert_lf_to_crlf`: whether a checkout under `s` changes this content.
    pub fn smudges(&self, s: Smudge) -> bool {
        match s {
            Smudge::None => false,
            Smudge::Crlf => self.lone_lf > 0,
            Smudge::AutoCrlf => {
                self.lone_lf > 0 && self.lone_cr == 0 && self.crlf == 0 && !self.is_binary()
            }
        }
    }
}

/// What a checkout under `s` writes for the stored `data`; borrowed when nothing changes.
pub fn to_worktree(data: &[u8], s: Smudge) -> Cow<'_, [u8]> {
    let stats = Stats::of(data);
    if !stats.smudges(s) {
        return Cow::Borrowed(data);
    }
    let lone_lf = usize::try_from(stats.lone_lf).unwrap_or(0);
    let mut out = Vec::with_capacity(data.len() + lone_lf);
    LfToCrlf::default().convert(data, &mut out);
    Cow::Owned(out)
}

/// The LF to CRLF rewrite of a checkout, fed one piece of the content at a time: a CR at the end
/// of one piece pairs with an LF at the start of the next.
#[derive(Clone, Copy, Debug, Default)]
pub struct LfToCrlf {
    prev: u8,
}

impl LfToCrlf {
    /// Appends the rewrite of the next piece `data` to `out`.
    pub fn convert(&mut self, data: &[u8], out: &mut Vec<u8>) {
        for &b in data {
            if b == b'\n' && self.prev != b'\r' {
                out.push(b'\r');
            }
            out.push(b);
            self.prev = b;
        }
    }
}

/// What is stored for the work tree content `data` under the ingest normalisation `eol`;
/// borrowed when nothing changes.
pub fn to_git(data: &[u8], eol: Eol) -> Cow<'_, [u8]> {
    if eol == Eol::Keep || !data.windows(2).any(|w| w == b"\r\n") {
        return Cow::Borrowed(data);
    }
    let nul = data.iter().take(BINARY_PROBE).any(|&b| b == 0);
    let lone_cr = data
        .iter()
        .enumerate()
        .any(|(i, &b)| b == b'\r' && data.get(i + 1) != Some(&b'\n'));
    if eol == Eol::Auto && (nul || lone_cr) {
        return Cow::Borrowed(data);
    }
    let mut out = Vec::with_capacity(data.len());
    for (i, &b) in data.iter().enumerate() {
        if b == b'\r' && data.get(i + 1) == Some(&b'\n') {
            continue;
        }
        out.push(b);
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_count_like_git() {
        let s = Stats::of(b"a\r\nb\nc\rd\0\x1a");
        assert_eq!((s.crlf, s.lone_lf, s.lone_cr, s.nul), (1, 1, 1, 1));
        assert!(s.is_binary());
        // A CR split across two reads is still one CRLF.
        let mut t = Stats::default();
        t.feed(b"a\r");
        t.feed(b"\nb");
        let t = t.finish();
        assert_eq!((t.crlf, t.lone_cr), (1, 0));
        assert_eq!(Stats::of(b"x\r").lone_cr, 1);
        // A final ^Z is not counted against the text.
        assert!(!Stats::of(b"text\n\x1a").is_binary());
        assert!(Stats::of(b"a\x01\x02").is_binary());
    }

    #[test]
    fn checkout_writes_crlf_where_git_does() {
        assert_eq!(&*to_worktree(b"a\nb\n", Smudge::Crlf), b"a\r\nb\r\n");
        assert_eq!(&*to_worktree(b"a\r\nb\n", Smudge::Crlf), b"a\r\nb\r\n");
        assert_eq!(&*to_worktree(b"a\nb\n", Smudge::None), b"a\nb\n");
        // auto leaves content alone that already has a CR, or is binary.
        assert_eq!(&*to_worktree(b"a\nb\n", Smudge::AutoCrlf), b"a\r\nb\r\n");
        assert_eq!(&*to_worktree(b"a\r\nb\n", Smudge::AutoCrlf), b"a\r\nb\n");
        assert_eq!(&*to_worktree(b"a\n\0", Smudge::AutoCrlf), b"a\n\0");
    }

    #[test]
    fn lf_to_crlf_pairs_a_cr_across_pieces() {
        let mut conv = LfToCrlf::default();
        let mut out = Vec::new();
        conv.convert(b"a\r", &mut out);
        conv.convert(b"\nb\n", &mut out);
        conv.convert(b"\n", &mut out);
        assert_eq!(out, b"a\r\nb\r\n\r\n");
        assert!(matches!(to_worktree(b"none", Smudge::Crlf), Cow::Borrowed(_)));
    }

    #[test]
    fn ingest_drops_crlf_unless_auto_sees_binary() {
        assert_eq!(&*to_git(b"a\r\nb\r\n", Eol::Text), b"a\nb\n");
        assert_eq!(&*to_git(b"a\r\nb\rc", Eol::Text), b"a\nb\rc");
        assert_eq!(&*to_git(b"a\r\nb\rc", Eol::Auto), b"a\r\nb\rc");
        assert_eq!(&*to_git(b"a\r\n\0", Eol::Auto), b"a\r\n\0");
        assert_eq!(&*to_git(b"a\r\n", Eol::Keep), b"a\r\n");
    }
}
