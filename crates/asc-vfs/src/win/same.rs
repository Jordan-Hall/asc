//! Whether a file the user wrote holds the old tree's version (SPEC §10.2 item 8, step 3).
//!
//! Git calls a path dirty when its bytes differ from what the work tree was checked out at, not
//! when it was written. A file edited and then committed, or changed and changed back, is clean
//! for the next switch. The placeholder state of both Windows backends only says that a file was
//! written, so before a switch leaves such a file alone it compares the bytes.

use std::io::{self, Read};

use asc_core::git::BlobOidHasher;
use asc_core::object::Kind;

use crate::core::Node;

/// Bytes read per call.
const CHUNK: usize = 64 * 1024;

/// Whether the `len` bytes of `r` are the content of the file `node`. A link, a directory or a
/// different size is never the same.
pub fn holds(r: &mut impl Read, len: u64, node: &Node) -> io::Result<bool> {
    if !matches!(node.kind, Kind::File | Kind::Exec) || len != node.size {
        return Ok(false);
    }
    let mut hasher = BlobOidHasher::new(len);
    let mut buf = vec![0u8; CHUNK];
    let mut seen = 0u64;
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        seen += n as u64;
        if seen > len {
            return Ok(false);
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().is_ok_and(|oid| oid == node.git))
}

#[cfg(test)]
mod tests {
    use super::*;
    use asc_core::git::blob_oid;
    use asc_core::path::RepoPath;

    fn node(kind: Kind, body: &[u8]) -> Node {
        Node {
            path: RepoPath::new(b"f".to_vec()).unwrap(),
            kind,
            id: [7; 32],
            size: body.len() as u64,
            git: blob_oid(body),
            read_only: false,
        }
    }

    #[test]
    fn the_same_bytes_are_the_same_file() {
        let n = node(Kind::File, b"hello\n");
        assert!(holds(&mut &b"hello\n"[..], 6, &n).unwrap());
        assert!(holds(&mut &b"hello\n"[..], 6, &node(Kind::Exec, b"hello\n")).unwrap());
    }

    #[test]
    fn other_bytes_another_size_or_a_link_are_not() {
        let n = node(Kind::File, b"hello\n");
        assert!(!holds(&mut &b"hellO\n"[..], 6, &n).unwrap());
        assert!(!holds(&mut &b"hello\n!"[..], 7, &n).unwrap());
        // more bytes than the declared length
        assert!(!holds(&mut &b"hello\n!"[..], 6, &n).unwrap());
        assert!(!holds(&mut &b"hello\n"[..], 6, &node(Kind::Symlink, b"hello\n")).unwrap());
    }

    #[test]
    fn a_large_file_is_read_in_chunks() {
        let body = vec![b'x'; CHUNK * 3 + 17];
        assert!(holds(&mut &body[..], body.len() as u64, &node(Kind::File, &body)).unwrap());
    }
}
