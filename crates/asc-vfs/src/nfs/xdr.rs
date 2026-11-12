//! XDR (RFC 4506): big-endian 4-byte units, variable data padded to a multiple of four. Every
//! read is bounded by what is left of the message and by the caller's maximum, so a hostile
//! length never allocates more than the message holds.

/// The message ended early or a length broke a bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Garbage;

pub type XResult<T> = std::result::Result<T, Garbage>;

/// Reads XDR from a byte slice.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

fn pad(n: usize) -> usize {
    (4 - n % 4) % 4
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes not read yet.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> XResult<&'a [u8]> {
        if n > self.remaining() {
            return Err(Garbage);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn u32(&mut self) -> XResult<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&mut self) -> XResult<u64> {
        let hi = u64::from(self.u32()?);
        let lo = u64::from(self.u32()?);
        Ok(hi << 32 | lo)
    }

    pub fn bool(&mut self) -> XResult<bool> {
        match self.u32()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Garbage),
        }
    }

    /// Fixed-length opaque data.
    pub fn fixed(&mut self, n: usize) -> XResult<&'a [u8]> {
        let s = self.take(n)?;
        self.take(pad(n))?;
        Ok(s)
    }

    /// Variable-length opaque data of at most `max` bytes.
    pub fn opaque(&mut self, max: usize) -> XResult<&'a [u8]> {
        let n = usize::try_from(self.u32()?).map_err(|_| Garbage)?;
        if n > max {
            return Err(Garbage);
        }
        self.fixed(n)
    }

    /// An optional value: a bool, then the value when it is `true`.
    pub fn option<T>(&mut self, f: impl FnOnce(&mut Self) -> XResult<T>) -> XResult<Option<T>> {
        if self.bool()? { f(self).map(Some) } else { Ok(None) }
    }
}

/// Writes XDR into a growing buffer.
#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(n: usize) -> Self {
        Self {
            buf: Vec::with_capacity(n),
        }
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u32(u32::from(v))
    }

    /// Fixed-length opaque data.
    pub fn fixed(&mut self, b: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(b);
        self.buf.extend(std::iter::repeat_n(0u8, pad(b.len())));
        self
    }

    /// Bytes already in XDR form.
    pub fn raw(&mut self, b: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(b);
        self
    }

    /// Variable-length opaque data (or a string). Callers keep `b` under 4 GiB.
    pub fn opaque(&mut self, b: &[u8]) -> &mut Self {
        self.u32(u32::try_from(b.len()).unwrap_or(u32::MAX));
        self.fixed(b)
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_type_with_padding() {
        let mut w = Writer::new();
        w.u32(7)
            .u64(0x0102_0304_0506_0708)
            .bool(true)
            .opaque(b"abcde")
            .fixed(b"xy");
        let b = w.into_bytes();
        assert_eq!(b.len(), 4 + 8 + 4 + (4 + 8) + 4);
        let mut r = Reader::new(&b);
        assert_eq!(r.u32(), Ok(7));
        assert_eq!(r.u64(), Ok(0x0102_0304_0506_0708));
        assert_eq!(r.bool(), Ok(true));
        assert_eq!(r.opaque(16), Ok(&b"abcde"[..]));
        assert_eq!(r.fixed(2), Ok(&b"xy"[..]));
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn refuses_short_messages_long_lengths_and_bad_bools() {
        let mut r = Reader::new(&[0, 0, 0]);
        assert_eq!(r.u32(), Err(Garbage));
        // A length beyond the maximum, and one beyond the message.
        let b = [0, 0, 0, 9, b'a', b'b', b'c', b'd'];
        assert_eq!(Reader::new(&b).opaque(8), Err(Garbage));
        assert_eq!(Reader::new(&b).opaque(100), Err(Garbage));
        let huge = [0xff, 0xff, 0xff, 0xff];
        assert_eq!(Reader::new(&huge).opaque(usize::MAX), Err(Garbage));
        assert_eq!(Reader::new(&[0, 0, 0, 2]).bool(), Err(Garbage));
    }
}
