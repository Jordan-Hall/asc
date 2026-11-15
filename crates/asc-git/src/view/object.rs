//! Loose-object renderer: the bytes of `objects/<2 hex>/<38 hex>` for one git object.
//!
//! A loose object is a zlib stream over `<kind> <len>\0<body>` (the *payload*). git does not care
//! how the stream was deflated, only that it inflates to the payload whose SHA-1 is the object's
//! name. The view therefore writes the zlib stream with stored (level 0) deflate blocks, which makes
//! the file a pure function of the body:
//!
//! ```text
//! 78 01                      zlib header (CM 8, 32 KiB window, no dictionary, FLEVEL 0)
//! block*                     one per at most 65,535 payload bytes, at least one
//!   BFINAL|BTYPE=00  (1)     0x01 for the last block, 0x00 before it
//!   LEN (2, LE)  NLEN (2, LE, the complement of LEN)
//!   LEN payload bytes
//! ADLER32 (4, BE)            of the whole payload
//! ```
//!
//! The framed size is `2 + payload + 5 * blocks + 4`, so [`LooseObject::size`] answers `getattr`
//! from the body length (a tree entry's `size` for a blob) without reading the body. A byte range
//! is answered by [`LooseObject::read`] from the body bytes [`LooseObject::body_span`] names. Only a
//! range that reaches the trailer needs the Adler-32 of the payload, which [`Adler32`] computes
//! incrementally over [`LooseObject::header`] and then the body; the caller caches it per oid.
//!
//! Nothing here reads a file or touches asc's state. The body comes from `asc_core::git`
//! (`render_tree`, `export_commit`, `export_tag`, `render_tag`, or a blob's bytes), so the object
//! git reads is byte-identical to what `asc export --git` stores.

use std::ops::Range;

pub use asc_core::git::ObjectKind;

/// The most payload bytes one stored deflate block holds (the 16-bit `LEN` field).
pub const MAX_BLOCK: u64 = 65_535;

/// The zlib header: `CMF = 0x78` (deflate, 32 KiB window), `FLG = 0x01` (no dictionary, fastest
/// level, `FCHECK` such that `0x7801` is divisible by 31).
const ZLIB_HEADER: [u8; 2] = [0x78, 0x01];
/// Bytes before the first block.
const PREFIX: u64 = ZLIB_HEADER.len() as u64;
/// Bytes of one stored-block header: the `BFINAL`/`BTYPE` byte, `LEN` and `NLEN`.
const BLOCK_HEADER: u64 = 5;
/// Bytes of the Adler-32 trailer.
const TRAILER: u64 = 4;
/// File bytes from one block's start to the next full block's start.
const STRIDE: u64 = BLOCK_HEADER + MAX_BLOCK;

/// Why a loose object could not be framed or read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    /// The framed object does not fit in 64 bits (or in memory, for [`render`]).
    #[error("object of {0} body bytes is too large to frame")]
    TooLarge(u64),
    /// The body bytes handed to [`LooseObject::read`] do not cover [`LooseObject::body_span`].
    #[error("body bytes cover {have} of the {need} the range needs")]
    ShortBody { need: u64, have: u64 },
    /// The range reaches the Adler-32 trailer but no checksum was given.
    #[error("the range reaches the Adler-32 trailer and no checksum was given")]
    ChecksumMissing,
}

/// Incremental Adler-32 (RFC 1950 section 8.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Adler32 {
    a: u32,
    b: u32,
}

impl Default for Adler32 {
    fn default() -> Self {
        Self::new()
    }
}

impl Adler32 {
    const MODULUS: u32 = 65_521;
    /// The most bytes that can be summed before `b` can overflow a `u32`.
    const NMAX: usize = 5_552;

    pub const fn new() -> Self {
        Self { a: 1, b: 0 }
    }

    pub fn update(&mut self, bytes: &[u8]) -> &mut Self {
        for chunk in bytes.chunks(Self::NMAX) {
            for &byte in chunk {
                self.a += u32::from(byte);
                self.b += self.a;
            }
            self.a %= Self::MODULUS;
            self.b %= Self::MODULUS;
        }
        self
    }

    pub const fn finish(self) -> u32 {
        (self.b << 16) | self.a
    }
}

/// The git name of an object kind, as it appears in the payload header.
const fn kind_name(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::Tree => "tree",
        ObjectKind::Commit => "commit",
        ObjectKind::Tag => "tag",
    }
}

/// The layout of one object's loose file, computed from its kind and body length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LooseObject {
    header: Vec<u8>,
    body_len: u64,
    payload_len: u64,
    blocks: u64,
    size: u64,
}

impl LooseObject {
    /// The layout of a `kind` object whose body is `body_len` bytes.
    pub fn new(kind: ObjectKind, body_len: u64) -> Result<Self, RenderError> {
        let too_large = || RenderError::TooLarge(body_len);
        let header = format!("{} {body_len}\0", kind_name(kind)).into_bytes();
        let payload_len = (header.len() as u64)
            .checked_add(body_len)
            .ok_or_else(too_large)?;
        // A payload is never empty (the header is), so there is at least one block.
        let blocks = payload_len.div_ceil(MAX_BLOCK);
        let size = blocks
            .checked_mul(BLOCK_HEADER)
            .and_then(|n| n.checked_add(payload_len))
            .and_then(|n| n.checked_add(PREFIX + TRAILER))
            .ok_or_else(too_large)?;
        Ok(Self {
            header,
            body_len,
            payload_len,
            blocks,
            size,
        })
    }

    /// The size of the loose file in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The length of the body this layout was made for.
    pub fn body_len(&self) -> u64 {
        self.body_len
    }

    /// The payload header, `<kind> <len>\0`.
    pub fn header(&self) -> &[u8] {
        &self.header
    }

    /// An Adler-32 that has already consumed [`header`](Self::header). Feed it every body byte,
    /// in order, and pass `finish()` to [`read`](Self::read).
    pub fn checksum(&self) -> Adler32 {
        let mut adler = Adler32::new();
        adler.update(&self.header);
        adler
    }

    /// File offset where the Adler-32 trailer starts.
    fn trailer_start(&self) -> u64 {
        self.size - TRAILER
    }

    /// The payload bytes that precede file offset `pos`.
    fn cursor(&self, pos: u64) -> u64 {
        if pos <= PREFIX {
            return 0;
        }
        if pos >= self.trailer_start() {
            return self.payload_len;
        }
        let q = pos - PREFIX;
        let (block, within) = (q / STRIDE, q % STRIDE);
        block * MAX_BLOCK
            + within
                .saturating_sub(BLOCK_HEADER)
                .min(self.block_len(block))
    }

    /// The payload bytes in block `block`.
    fn block_len(&self, block: u64) -> u64 {
        (self.payload_len - block * MAX_BLOCK).min(MAX_BLOCK)
    }

    /// Whether a read of `len` bytes at `offset` reaches the Adler-32 trailer.
    pub fn needs_checksum(&self, offset: u64, len: usize) -> bool {
        len > 0 && offset < self.size && offset.saturating_add(len as u64) > self.trailer_start()
    }

    /// The body bytes a read of `len` bytes at `offset` needs, as a range of the body. Empty when
    /// the range lies in the framing only, or past the end of the file.
    pub fn body_span(&self, offset: u64, len: usize) -> Range<u64> {
        let end = offset.saturating_add(len as u64).min(self.size);
        if offset >= end {
            return 0..0;
        }
        let header = self.header.len() as u64;
        let first = self.cursor(offset).saturating_sub(header);
        let last = self.cursor(end).saturating_sub(header);
        first..last.max(first)
    }

    /// The header bytes of block `block`.
    fn block_header(&self, block: u64) -> [u8; BLOCK_HEADER as usize] {
        let len = self.block_len(block) as u16;
        let [lo, hi] = len.to_le_bytes();
        let [nlo, nhi] = (!len).to_le_bytes();
        [u8::from(block + 1 == self.blocks), lo, hi, nlo, nhi]
    }

    /// Copies payload bytes `from..to` into the front of `out`. `body` starts at body offset `base`.
    fn copy_payload(&self, from: u64, to: u64, out: &mut [u8], body: &[u8], base: u64) {
        let header = self.header.len() as u64;
        let mut at = 0;
        if from < header {
            let stop = to.min(header);
            let n = (stop - from) as usize;
            out[..n].copy_from_slice(&self.header[from as usize..stop as usize]);
            at = n;
        }
        let body_from = from.max(header) - header;
        let body_to = to.max(header) - header;
        if body_to > body_from {
            let n = (body_to - body_from) as usize;
            let src = (body_from - base) as usize;
            out[at..at + n].copy_from_slice(&body[src..src + n]);
        }
    }

    /// Fills `out` with the loose file's bytes from `offset` and returns how many were written
    /// (fewer than `out.len()` only at the end of the file, zero at or past it).
    ///
    /// `body` holds the body bytes from `body_span(offset, out.len()).start`; it may be longer than
    /// the span. `checksum` is the finished [`Adler32`] of header and body and is required only when
    /// [`needs_checksum`](Self::needs_checksum) says so.
    pub fn read(
        &self,
        offset: u64,
        out: &mut [u8],
        body: &[u8],
        checksum: Option<u32>,
    ) -> Result<usize, RenderError> {
        let end = offset.saturating_add(out.len() as u64).min(self.size);
        if offset >= end {
            return Ok(0);
        }
        let span = self.body_span(offset, out.len());
        let need = span.end - span.start;
        if (body.len() as u64) < need {
            return Err(RenderError::ShortBody {
                need,
                have: body.len() as u64,
            });
        }
        if self.needs_checksum(offset, out.len()) && checksum.is_none() {
            return Err(RenderError::ChecksumMissing);
        }
        let mut pos = offset;
        while pos < end {
            let at = (pos - offset) as usize;
            let want = end - pos;
            let n = if pos < PREFIX {
                let n = want.min(PREFIX - pos);
                let from = pos as usize;
                out[at..at + n as usize].copy_from_slice(&ZLIB_HEADER[from..from + n as usize]);
                n
            } else if pos >= self.trailer_start() {
                let sum = checksum.ok_or(RenderError::ChecksumMissing)?.to_be_bytes();
                let skip = (pos - self.trailer_start()) as usize;
                let n = want.min((sum.len() - skip) as u64);
                out[at..at + n as usize].copy_from_slice(&sum[skip..skip + n as usize]);
                n
            } else {
                let q = pos - PREFIX;
                let (block, within) = (q / STRIDE, q % STRIDE);
                if within < BLOCK_HEADER {
                    let head = self.block_header(block);
                    let n = want.min(BLOCK_HEADER - within);
                    let from = within as usize;
                    out[at..at + n as usize].copy_from_slice(&head[from..from + n as usize]);
                    n
                } else {
                    let data = within - BLOCK_HEADER;
                    let from = block * MAX_BLOCK + data;
                    let n = want.min(self.block_len(block) - data);
                    self.copy_payload(from, from + n, &mut out[at..], body, span.start);
                    n
                }
            };
            pos += n;
        }
        Ok((end - offset) as usize)
    }
}

/// The whole loose file for `body`: a convenience over [`LooseObject`] for callers that hold the
/// body in memory.
pub fn render(kind: ObjectKind, body: &[u8]) -> Result<Vec<u8>, RenderError> {
    let layout = LooseObject::new(kind, body.len() as u64)?;
    let mut adler = layout.checksum();
    adler.update(body);
    let size =
        usize::try_from(layout.size()).map_err(|_| RenderError::TooLarge(layout.body_len()))?;
    let mut out = vec![0; size];
    let n = layout.read(0, &mut out, body, Some(adler.finish()))?;
    debug_assert_eq!(n, size);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inflates a stream of stored blocks and checks every structural field, so the tests do not
    /// depend on the renderer's own arithmetic.
    fn unstore(file: &[u8]) -> Vec<u8> {
        assert_eq!(file[..2], ZLIB_HEADER);
        assert_eq!((u32::from(file[0]) << 8 | u32::from(file[1])) % 31, 0);
        let mut pos = 2;
        let mut payload = Vec::new();
        loop {
            let last = match file[pos] {
                0 => false,
                1 => true,
                other => panic!("block header byte {other:#x}"),
            };
            let len = usize::from(u16::from_le_bytes([file[pos + 1], file[pos + 2]]));
            let nlen = u16::from_le_bytes([file[pos + 3], file[pos + 4]]);
            assert_eq!(nlen, !(len as u16));
            payload.extend_from_slice(&file[pos + 5..pos + 5 + len]);
            pos += 5 + len;
            if last {
                break;
            }
        }
        let sum = u32::from_be_bytes(file[pos..pos + 4].try_into().unwrap());
        assert_eq!(pos + 4, file.len());
        assert_eq!(sum, Adler32::new().update(&payload).finish());
        payload
    }

    fn body_of(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 % 251) as u8).collect()
    }

    #[test]
    fn view_object_adler32_matches_the_rfc_vectors() {
        assert_eq!(Adler32::new().finish(), 1);
        assert_eq!(Adler32::new().update(b"Wikipedia").finish(), 0x11E6_0398);
        assert_eq!(Adler32::new().update(b"a").finish(), 0x0062_0062);
        // 0xff bytes push both sums to the modulus quickly: a long run checks the NMAX split.
        let long = vec![0xff; 100_000];
        let mut slow = (1u64, 0u64);
        for &byte in &long {
            slow.0 = (slow.0 + u64::from(byte)) % 65_521;
            slow.1 = (slow.1 + slow.0) % 65_521;
        }
        assert_eq!(
            Adler32::new().update(&long).finish(),
            ((slow.1 << 16) | slow.0) as u32
        );
        // Chunking does not change the result.
        let mut parts = Adler32::new();
        for chunk in long.chunks(777) {
            parts.update(chunk);
        }
        assert_eq!(parts.finish(), Adler32::new().update(&long).finish());
    }

    #[test]
    fn view_object_size_is_a_function_of_the_body_length() {
        // `blob 0\0` is 7 payload bytes: 2 + 5 + 7 + 4.
        assert_eq!(LooseObject::new(ObjectKind::Blob, 0).unwrap().size(), 18);
        for (kind, len) in [
            (ObjectKind::Blob, 0),
            (ObjectKind::Blob, 1),
            (ObjectKind::Blob, 65_524),
            (ObjectKind::Blob, 65_525),
            (ObjectKind::Blob, 70_000),
            (ObjectKind::Tree, 131_070),
            (ObjectKind::Commit, 300_000),
            (ObjectKind::Tag, 42),
        ] {
            let body = body_of(len);
            let file = render(kind, &body).unwrap();
            let layout = LooseObject::new(kind, len as u64).unwrap();
            assert_eq!(layout.size(), file.len() as u64, "{kind:?} {len}");
            let payload = unstore(&file);
            let head = layout.header().len();
            assert_eq!(payload.len(), head + len);
            assert_eq!(&payload[..head], layout.header());
            assert_eq!(&payload[head..], &body[..]);
        }
    }

    #[test]
    fn view_object_block_boundaries_are_exact() {
        // `blob 65524\0` is 11 bytes, so a 65,524 byte body makes exactly one full block and one
        // more body byte starts a second block.
        let one = LooseObject::new(ObjectKind::Blob, 65_524).unwrap();
        assert_eq!(one.size(), 2 + 5 + 65_535 + 4);
        let two = LooseObject::new(ObjectKind::Blob, 65_525).unwrap();
        assert_eq!(two.size(), 2 + 2 * 5 + 65_536 + 4);
        let file = render(ObjectKind::Blob, &body_of(65_525)).unwrap();
        assert_eq!(file[2], 0, "first block is not final");
        assert_eq!(&file[3..5], &[0xff, 0xff]);
        let second = 2 + 5 + 65_535;
        assert_eq!(file[second], 1, "second block is final");
        assert_eq!(&file[second + 1..second + 5], &[1, 0, 0xfe, 0xff]);
    }

    #[test]
    fn view_object_any_range_equals_the_slice_of_the_whole_file() {
        for (kind, len) in [
            (ObjectKind::Blob, 0),
            (ObjectKind::Blob, 3),
            (ObjectKind::Blob, 65_524),
            (ObjectKind::Blob, 65_525),
            (ObjectKind::Blob, 70_000),
            (ObjectKind::Tree, 140_000),
        ] {
            let body = body_of(len);
            let whole = render(kind, &body).unwrap();
            let layout = LooseObject::new(kind, len as u64).unwrap();
            let mut adler = layout.checksum();
            adler.update(&body);
            let sum = adler.finish();
            let size = whole.len() as u64;
            let mut offsets = vec![
                0,
                1,
                2,
                3,
                6,
                7,
                8,
                size - 5,
                size - 4,
                size - 3,
                size - 1,
                size,
                size + 9,
            ];
            for edge in [65_542u64, 131_082] {
                offsets.extend([edge - 6, edge - 5, edge - 1, edge, edge + 1, edge + 6]);
            }
            for &offset in &offsets {
                for want in [0usize, 1, 2, 4, 5, 6, 100, 65_535, 65_541, 200_000] {
                    let span = layout.body_span(offset, want);
                    let slice = &body[span.start as usize..span.end as usize];
                    let mut out = vec![0xaa; want];
                    let n = layout.read(offset, &mut out, slice, Some(sum)).unwrap();
                    let from = offset.min(size) as usize;
                    let to = offset.saturating_add(want as u64).min(size) as usize;
                    let at = format!("{kind:?} {len} @{offset}+{want}");
                    assert_eq!(n, to - from, "{at}");
                    assert_eq!(&out[..n], &whole[from..to], "{at}");
                    // The span is tight: one byte less is a short body.
                    if !slice.is_empty() {
                        let short = &slice[..slice.len() - 1];
                        assert!(
                            matches!(
                                layout.read(offset, &mut out, short, Some(sum)),
                                Err(RenderError::ShortBody { .. })
                            ),
                            "{at}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn view_object_reads_in_steps_rebuild_the_file() {
        let body = body_of(140_000);
        let whole = render(ObjectKind::Blob, &body).unwrap();
        let layout = LooseObject::new(ObjectKind::Blob, 140_000).unwrap();
        let mut adler = layout.checksum();
        adler.update(&body);
        for step in [1usize, 7, 4096, 65_536] {
            let mut rebuilt = Vec::new();
            let mut buf = vec![0; step];
            loop {
                let offset = rebuilt.len() as u64;
                let span = layout.body_span(offset, step);
                let slice = &body[span.start as usize..span.end as usize];
                let n = layout
                    .read(offset, &mut buf, slice, Some(adler.finish()))
                    .unwrap();
                if n == 0 {
                    break;
                }
                rebuilt.extend_from_slice(&buf[..n]);
            }
            assert_eq!(rebuilt, whole, "step {step}");
        }
    }

    #[test]
    fn view_object_the_trailer_needs_the_checksum_and_nothing_else_does() {
        let body = body_of(10);
        let layout = LooseObject::new(ObjectKind::Blob, 10).unwrap();
        let size = layout.size();
        assert!(!layout.needs_checksum(0, (size - 4) as usize));
        assert!(layout.needs_checksum(0, size as usize));
        assert!(layout.needs_checksum(size - 1, 1));
        assert!(!layout.needs_checksum(size, 4));
        assert!(!layout.needs_checksum(0, 0));
        let mut out = vec![0; size as usize];
        assert_eq!(
            layout.read(0, &mut out, &body, None),
            Err(RenderError::ChecksumMissing)
        );
        let mut head = vec![0; (size - 4) as usize];
        assert_eq!(layout.read(0, &mut head, &body, None), Ok(head.len()));
    }

    #[test]
    fn view_object_a_range_in_the_framing_needs_no_body() {
        let layout = LooseObject::new(ObjectKind::Blob, 100_000).unwrap();
        assert_eq!(layout.body_span(0, 0), 0..0);
        assert_eq!(layout.body_span(0, 2), 0..0, "zlib header");
        assert_eq!(layout.body_span(2, 5), 0..0, "first block header");
        assert_eq!(layout.body_span(layout.size(), 10), 0..0);
        assert_eq!(layout.body_span(layout.size() - 4, 4), 100_000..100_000);
        assert_eq!(layout.body_span(0, usize::MAX), 0..100_000);
    }

    #[test]
    fn view_object_a_body_that_overflows_the_framing_is_refused() {
        assert_eq!(
            LooseObject::new(ObjectKind::Blob, u64::MAX),
            Err(RenderError::TooLarge(u64::MAX))
        );
        let near = u64::MAX - 1_000_000;
        assert_eq!(
            LooseObject::new(ObjectKind::Blob, near),
            Err(RenderError::TooLarge(near))
        );
        assert!(LooseObject::new(ObjectKind::Blob, 1 << 40).is_ok());
    }
}
