//! Ingest-side line-ending normalisation (SPEC §2.4.1) and hashing of working files. The
//! decision matches `asc_wc::status`, which hashes dirty files the same way, so a file `add`
//! staged is clean in `status`.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use asc_core::content::{ContentError, Ingested, ingest};
use asc_core::git::BlobOidHasher;
use asc_core::id::GitOid;
use asc_wc::status::Eol;

/// Bytes checked for NUL by git's binary heuristic.
const BINARY_PROBE: u64 = 8000;

/// What reading a file as `eol` yields.
#[derive(Clone, Copy, Debug)]
struct Plan {
    convert: bool,
    /// Length of the normalised content.
    size: u64,
    /// Length of the file when it was measured; reads stop there.
    len: u64,
}

fn plan(path: &Path, eol: Eol) -> io::Result<Plan> {
    let len = fs::metadata(path)?.len();
    if eol == Eol::Keep {
        return Ok(Plan {
            convert: false,
            size: len,
            len,
        });
    }
    let (mut total, mut crlf, mut lone_cr, mut nul, mut pending_cr) =
        (0u64, 0u64, false, false, false);
    let mut buf = vec![0u8; 64 * 1024];
    let mut file = File::open(path)?.take(len);
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for (i, &b) in buf[..n].iter().enumerate() {
            nul |= total + (i as u64) < BINARY_PROBE && b == 0;
            if pending_cr {
                if b == b'\n' {
                    crlf += 1;
                } else {
                    lone_cr = true;
                }
            }
            pending_cr = b == b'\r';
        }
        total += n as u64;
    }
    lone_cr |= pending_cr;
    let convert = eol == Eol::Text || (!nul && !lone_cr);
    Ok(Plan {
        convert,
        size: total - if convert { crlf } else { 0 },
        len,
    })
}

/// Feeds the normalised content of `path` to `sink`.
fn feed(path: &Path, plan: Plan, sink: &mut dyn FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
    let mut file = File::open(path)?.take(plan.len);
    let mut buf = vec![0u8; 64 * 1024];
    let mut out = Vec::with_capacity(buf.len());
    let mut pending_cr = false;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if !plan.convert {
            sink(&buf[..n])?;
            continue;
        }
        out.clear();
        for &b in &buf[..n] {
            if pending_cr {
                pending_cr = false;
                if b != b'\n' {
                    out.push(b'\r');
                }
            }
            if b == b'\r' {
                pending_cr = true;
            } else {
                out.push(b);
            }
        }
        sink(&out)?;
    }
    if pending_cr {
        sink(b"\r")?;
    }
    Ok(())
}

/// The git blob oid of `path` after normalisation, streamed.
pub fn blob_oid(path: &Path, eol: Eol) -> io::Result<GitOid> {
    let plan = plan(path, eol)?;
    let mut hasher = BlobOidHasher::new(plan.size);
    feed(path, plan, &mut |chunk| {
        hasher.update(chunk);
        Ok(())
    })?;
    hasher.finalize().map_err(io::Error::other)
}

/// Whether the file is executable; `None` where the file system has no exec bit.
#[cfg(unix)]
pub fn exec_bit(meta: &fs::Metadata) -> Option<bool> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o111 != 0)
}

/// Whether the file is executable; `None` where the file system has no exec bit.
#[cfg(not(unix))]
pub fn exec_bit(_: &fs::Metadata) -> Option<bool> {
    None
}

/// The bytes a symlink at `abs` stores: its target, `/`-separated.
pub fn link_target(abs: &Path) -> io::Result<Vec<u8>> {
    let target = fs::read_link(abs)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(target.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        Ok(target.to_string_lossy().replace('\\', "/").into_bytes())
    }
}

/// The oid of `bytes` as a blob.
pub fn bytes_oid(bytes: &[u8]) -> io::Result<GitOid> {
    let mut hasher = BlobOidHasher::new(bytes.len() as u64);
    hasher.update(bytes);
    hasher.finalize().map_err(io::Error::other)
}

/// A scratch file removed on drop.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Ingests `path` after normalisation. Converted content goes through a scratch file in
/// `tmp_dir`, so a large file is never held twice.
pub fn ingest_file(path: &Path, eol: Eol, tmp_dir: &Path) -> Result<Ingested, ContentError> {
    let plan = plan(path, eol)?;
    if !plan.convert {
        return ingest(File::open(path)?);
    }
    fs::create_dir_all(tmp_dir)?;
    let scratch = Scratch(tmp_dir.join(format!("add-{}.tmp", uuid::Uuid::now_v7())));
    {
        let mut out = io::BufWriter::new(File::create(&scratch.0)?);
        feed(path, plan, &mut |chunk| out.write_all(chunk))?;
        out.flush()?;
    }
    ingest(File::open(&scratch.0)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(dir: &Path, bytes: &[u8]) -> PathBuf {
        let p = dir.join("f");
        fs::write(&p, bytes).unwrap();
        p
    }

    fn oid(bytes: &[u8]) -> GitOid {
        asc_core::git::blob_oid(bytes)
    }

    #[test]
    fn text_converts_crlf_and_keeps_lone_cr() {
        let d = tempfile::tempdir().unwrap();
        let p = file(d.path(), b"a\r\nb\rc\r\n");
        assert_eq!(blob_oid(&p, Eol::Text).unwrap(), oid(b"a\nb\rc\n"));
        assert_eq!(blob_oid(&p, Eol::Keep).unwrap(), oid(b"a\r\nb\rc\r\n"));
        // A lone CR makes `text=auto` treat the file as binary.
        assert_eq!(blob_oid(&p, Eol::Auto).unwrap(), oid(b"a\r\nb\rc\r\n"));
    }

    #[test]
    fn auto_converts_text_and_skips_nul() {
        let d = tempfile::tempdir().unwrap();
        let p = file(d.path(), b"x\r\ny\r\n");
        assert_eq!(blob_oid(&p, Eol::Auto).unwrap(), oid(b"x\ny\n"));
        let p = file(d.path(), b"x\0\r\ny\r\n");
        assert_eq!(blob_oid(&p, Eol::Auto).unwrap(), oid(b"x\0\r\ny\r\n"));
    }

    #[test]
    fn ingest_agrees_with_the_streamed_oid() {
        let d = tempfile::tempdir().unwrap();
        let p = file(d.path(), b"one\r\ntwo\r\nend\r");
        let got = ingest_file(&p, Eol::Text, &d.path().join("tmp")).unwrap();
        assert_eq!(got.git, blob_oid(&p, Eol::Text).unwrap());
        assert_eq!(got.size, 12);
        assert!(
            !d.path().join("tmp").read_dir().unwrap().any(|_| true),
            "scratch removed"
        );
    }
}
