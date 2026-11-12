//! The FSKit wire protocol: what the asc FSKit module (`macos/fskit`) and the daemon say over the
//! work tree's Unix-domain socket. `docs/design/FSKIT.md` is the reference; this file and
//! `macos/fskit/Sources/AscWire/Wire.swift` must agree with it.
//!
//! Every message is one frame: a big-endian `u32` length, then that many bytes. A request is
//! `xid u32 ‖ op u32 ‖ arguments`; its reply is `xid u32 ‖ err u32 ‖ results`, with results only
//! when `err` is 0. Fields use the XDR encoding of [`crate::nfs::xdr`]: big-endian words, byte
//! strings as a length and the bytes padded to four. Replies may come in any order; the client
//! matches them by `xid`.

use std::io::{self, Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::nfs::xdr::{Reader, Writer, XResult};
use crate::unixfs::{Attr, Errno, VKind};

/// The protocol version [`op::HELLO`] agrees on.
pub const VERSION: u32 = 1;
/// The largest `READ` or `WRITE` payload.
pub const MAX_IO: u32 = 1 << 20;
/// The largest frame either side sends or accepts.
pub const MAX_FRAME: usize = MAX_IO as usize + 64 * 1024;
/// The longest name read off the wire; longer names are refused by the file system.
pub const MAX_WIRE_NAME: usize = 1024;

/// Operation numbers.
pub mod op {
    /// `version u32` → `version u32 ‖ root ino u64 ‖ attr ‖ max_io u32`.
    pub const HELLO: u32 = 0;
    /// `ino u64` → `attr`.
    pub const GETATTR: u32 = 1;
    /// `dir u64 ‖ name` → `attr`. Counts one lookup of the inode, released by `FORGET`.
    pub const LOOKUP: u32 = 2;
    /// `ino u64 ‖ n u64` → nothing. Releases `n` counted lookups.
    pub const FORGET: u32 = 3;
    /// `ino u64 ‖ mask u32 ‖ mode u32 ‖ uid u32 ‖ gid u32 ‖ size u64 ‖ atime ‖ mtime` → `attr`.
    pub const SETATTR: u32 = 4;
    /// `ino u64` → `target`.
    pub const READLINK: u32 = 5;
    /// `ino u64 ‖ offset u64 ‖ count u32` → `eof bool ‖ data`.
    pub const READ: u32 = 6;
    /// `ino u64 ‖ offset u64 ‖ data` → `count u32 ‖ attr`.
    pub const WRITE: u32 = 7;
    /// `dir u64 ‖ name ‖ mode u32` → `attr`. Counts one lookup.
    pub const CREATE: u32 = 8;
    /// `dir u64 ‖ name ‖ mode u32` → `attr`. Counts one lookup.
    pub const MKDIR: u32 = 9;
    /// `dir u64 ‖ name ‖ target` → `attr`. Counts one lookup.
    pub const SYMLINK: u32 = 10;
    /// `dir u64 ‖ name ‖ dir bool` → nothing.
    pub const REMOVE: u32 = 11;
    /// `from u64 ‖ name ‖ to u64 ‖ name ‖ flags u32` → nothing. Flag 1: fail when the target
    /// exists.
    pub const RENAME: u32 = 12;
    /// `dir u64 ‖ cookie u64 ‖ verifier u64 ‖ max u32 ‖ attrs bool` →
    /// `verifier u64 ‖ { more bool ‖ ino u64 ‖ name ‖ cookie u64 ‖ kind u32 ‖ [attr] }* ‖ eof bool`.
    pub const READDIR: u32 = 13;
    /// nothing → `blocks u64 ‖ free u64 ‖ avail u64 ‖ files u64 ‖ ffree u64 ‖ bsize u32 ‖
    /// namemax u32`.
    pub const STATFS: u32 = 14;
    /// `ino u64` → nothing. Flushes the file to stable storage.
    pub const SYNC: u32 = 15;
    /// `ino u64` → nothing. The client closed its last use of the file.
    pub const CLOSE: u32 = 16;
}

/// `SETATTR` mask bits.
pub mod set {
    pub const MODE: u32 = 1;
    pub const UID: u32 = 2;
    pub const GID: u32 = 4;
    pub const SIZE: u32 = 8;
    pub const ATIME: u32 = 0x10;
    pub const MTIME: u32 = 0x20;
}

/// `RENAME` flag: refuse to replace an existing target.
pub const RENAME_NOREPLACE: u32 = 1;

/// Item kinds.
pub mod kind {
    pub const FILE: u32 = 1;
    pub const DIR: u32 = 2;
    pub const SYMLINK: u32 = 3;
}

/// The error numbers on the wire: the Darwin values, because the client is macOS.
pub mod err {
    pub const PERM: u32 = 1;
    pub const NOENT: u32 = 2;
    pub const IO: u32 = 5;
    pub const BADF: u32 = 9;
    pub const ACCES: u32 = 13;
    pub const EXIST: u32 = 17;
    pub const XDEV: u32 = 18;
    pub const NOTDIR: u32 = 20;
    pub const ISDIR: u32 = 21;
    pub const INVAL: u32 = 22;
    pub const MFILE: u32 = 24;
    pub const FBIG: u32 = 27;
    pub const NOSPC: u32 = 28;
    pub const ROFS: u32 = 30;
    pub const MLINK: u32 = 31;
    pub const AGAIN: u32 = 35;
    pub const NOTSUP: u32 = 45;
    pub const LOOP: u32 = 62;
    pub const NAMETOOLONG: u32 = 63;
    pub const NOTEMPTY: u32 = 66;
    pub const DQUOT: u32 = 69;
    pub const STALE: u32 = 70;
    pub const NOSYS: u32 = 78;
    /// A protocol version this side does not speak.
    pub const RPCMISMATCH: u32 = 75;
}

/// The wire number of an error. Matched by name: the numbers differ between platforms.
pub fn err_of(e: Errno) -> u32 {
    match e {
        Errno::PERM => err::PERM,
        Errno::NOENT => err::NOENT,
        Errno::BADF => err::BADF,
        Errno::ACCESS => err::ACCES,
        Errno::EXIST => err::EXIST,
        Errno::XDEV => err::XDEV,
        Errno::NOTDIR => err::NOTDIR,
        Errno::ISDIR => err::ISDIR,
        Errno::INVAL => err::INVAL,
        Errno::MFILE => err::MFILE,
        Errno::FBIG => err::FBIG,
        Errno::NOSPC => err::NOSPC,
        Errno::ROFS => err::ROFS,
        Errno::MLINK => err::MLINK,
        Errno::AGAIN => err::AGAIN,
        Errno::NOTSUP => err::NOTSUP,
        Errno::LOOP => err::LOOP,
        Errno::NAMETOOLONG => err::NAMETOOLONG,
        Errno::NOTEMPTY => err::NOTEMPTY,
        Errno::DQUOT => err::DQUOT,
        Errno::STALE => err::STALE,
        _ => err::IO,
    }
}

/// The wire kind of a [`VKind`].
pub fn kind_of(k: VKind) -> u32 {
    match k {
        VKind::File => kind::FILE,
        VKind::Dir => kind::DIR,
        VKind::Symlink => kind::SYMLINK,
    }
}

/// A time on the wire: seconds since the epoch as a two's-complement `u64`, then nanoseconds.
pub fn put_time(w: &mut Writer, t: SystemTime) {
    let (s, n) = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(e) => {
            let d = e.duration();
            let s = -(d.as_secs() as i64);
            match d.subsec_nanos() {
                0 => (s, 0),
                n => (s - 1, 1_000_000_000 - n),
            }
        }
    };
    w.u64(s as u64).u32(n);
}

/// Reads a time written by [`put_time`].
pub fn get_time(r: &mut Reader<'_>) -> XResult<SystemTime> {
    let s = r.u64()? as i64;
    let n = r.u32()?.min(999_999_999);
    Ok(crate::unixfs::merged::sys_time(s, i64::from(n)))
}

/// Writes an item's attributes:
/// `ino u64 ‖ parent u64 ‖ kind u32 ‖ perm u32 ‖ nlink u32 ‖ uid u32 ‖ gid u32 ‖ size u64 ‖
/// alloc u64 ‖ atime ‖ mtime ‖ ctime`.
pub fn put_attr(w: &mut Writer, a: &Attr, parent: u64) {
    w.u64(a.ino)
        .u64(parent)
        .u32(kind_of(a.kind))
        .u32(u32::from(a.perm))
        .u32(a.nlink)
        .u32(a.uid)
        .u32(a.gid)
        .u64(a.size)
        .u64(a.blocks.saturating_mul(512));
    put_time(w, a.atime);
    put_time(w, a.mtime);
    put_time(w, a.ctime);
}

/// Reads one frame; `None` at a clean end of stream.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_be_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a frame of {n} bytes is over the limit of {MAX_FRAME}"),
        ));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

/// Writes one frame.
pub fn write_frame(w: &mut impl Write, body: &[u8]) -> io::Result<()> {
    let n = u32::try_from(body.len())
        .ok()
        .filter(|&n| n as usize <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&n.to_be_bytes());
    out.extend_from_slice(body);
    w.write_all(&out)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn times_round_trip_on_both_sides_of_the_epoch() {
        for t in [
            UNIX_EPOCH + Duration::new(1_700_000_000, 123),
            UNIX_EPOCH,
            UNIX_EPOCH - Duration::new(5, 250_000_000),
            UNIX_EPOCH - Duration::from_secs(9),
        ] {
            let mut w = Writer::new();
            put_time(&mut w, t);
            assert_eq!(w.len(), 12);
            assert_eq!(get_time(&mut Reader::new(w.bytes())).unwrap(), t);
        }
    }

    #[test]
    fn frames_round_trip_and_refuse_oversized_lengths() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello").unwrap();
        write_frame(&mut buf, b"").unwrap();
        let mut r = io::Cursor::new(buf);
        assert_eq!(read_frame(&mut r).unwrap().unwrap(), b"hello");
        assert_eq!(read_frame(&mut r).unwrap().unwrap(), b"");
        assert!(read_frame(&mut r).unwrap().is_none());
        let big = (MAX_FRAME as u32 + 1).to_be_bytes();
        assert!(read_frame(&mut io::Cursor::new(big.to_vec())).is_err());
        assert!(write_frame(&mut Vec::new(), &vec![0; MAX_FRAME + 1]).is_err());
        // A frame cut short is an error, not an end.
        let mut cut = 8u32.to_be_bytes().to_vec();
        cut.extend_from_slice(b"abc");
        assert!(read_frame(&mut io::Cursor::new(cut)).is_err());
    }

    #[test]
    fn errors_map_to_darwin_numbers() {
        assert_eq!(err_of(Errno::NOENT), 2);
        assert_eq!(err_of(Errno::NOTEMPTY), 66);
        assert_eq!(err_of(Errno::NAMETOOLONG), 63);
        assert_eq!(err_of(Errno::LOOP), 62);
        assert_eq!(err_of(Errno::NOTSUP), 45);
        assert_eq!(err_of(Errno::STALE), 70);
        assert_eq!(err_of(Errno::OVERFLOW), err::IO);
    }
}
