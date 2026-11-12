//! A minimal NFSv3 client over the wire, for the protocol tests.

use std::io::BufReader;
use std::net::{SocketAddr, TcpStream};

use super::rpc;
use super::server::{MOUNT_PROG, NFS_PROG};
use super::xdr::{Reader, Writer};

/// The attributes the tests look at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fattr {
    pub kind: u32,
    pub mode: u32,
    pub uid: u32,
    pub size: u64,
    pub fileid: u64,
    pub mtime: (u32, u32),
    pub ctime: (u32, u32),
}

pub fn fattr(r: &mut Reader<'_>) -> Fattr {
    let kind = r.u32().unwrap();
    let mode = r.u32().unwrap();
    let _nlink = r.u32().unwrap();
    let uid = r.u32().unwrap();
    let _gid = r.u32().unwrap();
    let size = r.u64().unwrap();
    let _used = r.u64().unwrap();
    r.u64().unwrap();
    let _fsid = r.u64().unwrap();
    let fileid = r.u64().unwrap();
    r.u64().unwrap();
    let mtime = (r.u32().unwrap(), r.u32().unwrap());
    let ctime = (r.u32().unwrap(), r.u32().unwrap());
    Fattr {
        kind,
        mode,
        uid,
        size,
        fileid,
        mtime,
        ctime,
    }
}

fn post_attr(r: &mut Reader<'_>) -> Option<Fattr> {
    r.bool().unwrap().then(|| fattr(r))
}

fn wcc(r: &mut Reader<'_>) -> Option<Fattr> {
    assert!(!r.bool().unwrap(), "no pre-operation attributes");
    post_attr(r)
}

/// `Ok(body after the status)` or `Err(nfsstat3)`.
pub type Res<T> = Result<T, u32>;

pub struct Client {
    out: TcpStream,
    input: BufReader<TcpStream>,
    xid: u32,
    pub cred: Option<(u32, u32)>,
}

pub type Fh = Vec<u8>;

impl Client {
    pub fn connect(addr: SocketAddr, uid: u32, gid: u32) -> Self {
        let s = TcpStream::connect(addr).unwrap();
        Self {
            input: BufReader::new(s.try_clone().unwrap()),
            out: s,
            xid: 0,
            cred: Some((uid, gid)),
        }
    }

    /// Sends a call with transaction id `xid`, without waiting.
    pub fn send(&mut self, xid: u32, prog: u32, vers: u32, proc_: u32, args: &[u8]) {
        let msg = rpc::call(xid, prog, vers, proc_, self.cred, args);
        rpc::write_record(&mut self.out, &msg).unwrap();
    }

    /// Reads one reply: its transaction id and the accepted body, or the refusal.
    pub fn recv(&mut self) -> (u32, rpc::Reply) {
        let r = rpc::read_record(&mut self.input).unwrap().unwrap();
        rpc::parse_reply(&r).unwrap()
    }

    pub fn call(&mut self, prog: u32, vers: u32, proc_: u32, args: &[u8]) -> rpc::Reply {
        self.xid += 1;
        let xid = self.xid;
        self.send(xid, prog, vers, proc_, args);
        let (got, body) = self.recv();
        assert_eq!(got, xid);
        body
    }

    /// An NFS call; answers the reply after the status, or the status.
    pub fn nfs(&mut self, proc_: u32, args: &Writer) -> (u32, Vec<u8>) {
        let body = self
            .call(NFS_PROG, 3, proc_, args.bytes())
            .expect("accepted");
        let mut r = Reader::new(&body);
        let st = r.u32().unwrap();
        (st, r.fixed(r.remaining()).unwrap().to_vec())
    }

    fn ok<T>(&mut self, proc_: u32, args: &Writer, f: impl FnOnce(&mut Reader<'_>) -> T) -> Res<T> {
        let (st, body) = self.nfs(proc_, args);
        if st != 0 {
            return Err(st);
        }
        let mut r = Reader::new(&body);
        let v = f(&mut r);
        assert_eq!(r.remaining(), 0, "the whole reply was read");
        Ok(v)
    }

    /// MNT of `path`: the root handle, or the `mountstat3`.
    pub fn mnt(&mut self, path: &str) -> Res<Fh> {
        let mut w = Writer::new();
        w.opaque(path.as_bytes());
        let body = self.call(MOUNT_PROG, 3, 1, w.bytes()).expect("accepted");
        let mut r = Reader::new(&body);
        match r.u32().unwrap() {
            0 => {
                let fh = r.opaque(64).unwrap().to_vec();
                assert_eq!(r.u32().unwrap(), 1);
                assert_eq!(r.u32().unwrap(), rpc::AUTH_SYS);
                Ok(fh)
            }
            s => Err(s),
        }
    }

    pub fn root(&mut self) -> Fh {
        self.mnt("/asc").unwrap()
    }

    fn fh_args(fh: &[u8]) -> Writer {
        let mut w = Writer::new();
        w.opaque(fh);
        w
    }

    fn dirop(dir: &[u8], name: &str) -> Writer {
        let mut w = Self::fh_args(dir);
        w.opaque(name.as_bytes());
        w
    }

    pub fn getattr(&mut self, fh: &[u8]) -> Res<Fattr> {
        self.ok(1, &Self::fh_args(fh), fattr)
    }

    /// SETATTR of the mode and size, with an optional ctime guard.
    pub fn setattr(
        &mut self,
        fh: &[u8],
        mode: Option<u32>,
        size: Option<u64>,
        guard: Option<(u32, u32)>,
    ) -> Res<Option<Fattr>> {
        let mut w = Self::fh_args(fh);
        match mode {
            Some(m) => w.bool(true).u32(m),
            None => w.bool(false),
        };
        w.bool(false).bool(false);
        match size {
            Some(s) => w.bool(true).u64(s),
            None => w.bool(false),
        };
        w.u32(0).u32(0);
        match guard {
            Some((s, n)) => w.bool(true).u32(s).u32(n),
            None => w.bool(false),
        };
        self.ok(2, &w, wcc)
    }

    pub fn lookup(&mut self, dir: &[u8], name: &str) -> Res<(Fh, Fattr)> {
        self.ok(3, &Self::dirop(dir, name), |r| {
            let fh = r.opaque(64).unwrap().to_vec();
            let a = post_attr(r).unwrap();
            post_attr(r);
            (fh, a)
        })
    }

    /// Walks `a/b/c` from the root.
    pub fn walk(&mut self, path: &str) -> Res<(Fh, Fattr)> {
        let mut fh = self.root();
        let mut attr = self.getattr(&fh)?;
        for n in path.split('/') {
            (fh, attr) = self.lookup(&fh, n)?;
        }
        Ok((fh, attr))
    }

    pub fn access(&mut self, fh: &[u8], want: u32) -> Res<u32> {
        let mut w = Self::fh_args(fh);
        w.u32(want);
        self.ok(4, &w, |r| {
            post_attr(r);
            r.u32().unwrap()
        })
    }

    pub fn readlink(&mut self, fh: &[u8]) -> Res<Vec<u8>> {
        self.ok(5, &Self::fh_args(fh), |r| {
            post_attr(r);
            r.opaque(4096).unwrap().to_vec()
        })
    }

    pub fn read(&mut self, fh: &[u8], offset: u64, count: u32) -> Res<(Vec<u8>, bool)> {
        let mut w = Self::fh_args(fh);
        w.u64(offset).u32(count);
        self.ok(6, &w, |r| {
            post_attr(r);
            let n = r.u32().unwrap();
            let eof = r.bool().unwrap();
            let d = r.opaque(1 << 21).unwrap().to_vec();
            assert_eq!(d.len(), n as usize);
            (d, eof)
        })
    }

    pub fn write(
        &mut self,
        fh: &[u8],
        offset: u64,
        data: &[u8],
        stable: u32,
    ) -> Res<(u32, u32, Vec<u8>)> {
        let mut w = Self::fh_args(fh);
        w.u64(offset)
            .u32(u32::try_from(data.len()).unwrap())
            .u32(stable)
            .opaque(data);
        self.ok(7, &w, |r| {
            wcc(r);
            let n = r.u32().unwrap();
            let committed = r.u32().unwrap();
            (n, committed, r.fixed(8).unwrap().to_vec())
        })
    }

    fn made(r: &mut Reader<'_>) -> (Fh, Fattr) {
        assert!(r.bool().unwrap());
        let fh = r.opaque(64).unwrap().to_vec();
        let a = post_attr(r).unwrap();
        wcc(r);
        (fh, a)
    }

    /// CREATE: `how` 0 unchecked, 1 guarded (both with `mode`), 2 exclusive with `verf`.
    pub fn create(
        &mut self,
        dir: &[u8],
        name: &str,
        how: u32,
        mode: u32,
        verf: [u8; 8],
    ) -> Res<(Fh, Fattr)> {
        let mut w = Self::dirop(dir, name);
        w.u32(how);
        if how == 2 {
            w.fixed(&verf);
        } else {
            w.bool(true)
                .u32(mode)
                .bool(false)
                .bool(false)
                .bool(false)
                .u32(0)
                .u32(0);
        }
        self.ok(8, &w, Self::made)
    }

    pub fn mkdir(&mut self, dir: &[u8], name: &str) -> Res<(Fh, Fattr)> {
        let mut w = Self::dirop(dir, name);
        w.bool(true)
            .u32(0o750)
            .bool(false)
            .bool(false)
            .bool(false)
            .u32(0)
            .u32(0);
        self.ok(9, &w, Self::made)
    }

    pub fn symlink(&mut self, dir: &[u8], name: &str, target: &str) -> Res<(Fh, Fattr)> {
        let mut w = Self::dirop(dir, name);
        w.bool(false)
            .bool(false)
            .bool(false)
            .bool(false)
            .u32(0)
            .u32(0);
        w.opaque(target.as_bytes());
        self.ok(10, &w, Self::made)
    }

    pub fn remove(&mut self, dir: &[u8], name: &str) -> Res<()> {
        self.ok(12, &Self::dirop(dir, name), |r| {
            wcc(r);
        })
    }

    pub fn rmdir(&mut self, dir: &[u8], name: &str) -> Res<()> {
        self.ok(13, &Self::dirop(dir, name), |r| {
            wcc(r);
        })
    }

    pub fn rename(&mut self, from: &[u8], fname: &str, to: &[u8], tname: &str) -> Res<()> {
        let mut w = Self::dirop(from, fname);
        w.opaque(to).opaque(tname.as_bytes());
        self.ok(14, &w, |r| {
            wcc(r);
            wcc(r);
        })
    }

    /// One READDIR (or READDIRPLUS) call: entries (name, fileid, cookie, attributes and
    /// handle), the verifier, and whether it reached the end.
    #[allow(clippy::type_complexity)]
    pub fn readdir_page(
        &mut self,
        dir: &[u8],
        cookie: u64,
        verf: u64,
        count: u32,
        plus: bool,
    ) -> Res<(Vec<(String, u64, u64, Option<(Fattr, Fh)>)>, u64, bool)> {
        let mut w = Self::fh_args(dir);
        w.u64(cookie).u64(verf).u32(count);
        if plus {
            w.u32(count);
        }
        self.ok(if plus { 17 } else { 16 }, &w, |r| {
            post_attr(r);
            let verf = r.u64().unwrap();
            let mut out = Vec::new();
            while r.bool().unwrap() {
                let id = r.u64().unwrap();
                let name = String::from_utf8(r.opaque(1024).unwrap().to_vec()).unwrap();
                let cookie = r.u64().unwrap();
                let extra = plus.then(|| {
                    let a = post_attr(r).unwrap();
                    assert!(r.bool().unwrap());
                    (a, r.opaque(64).unwrap().to_vec())
                });
                out.push((name, id, cookie, extra));
            }
            let eof = r.bool().unwrap();
            (out, verf, eof)
        })
    }

    /// Every name of a directory, following cookies with pages of `count` bytes.
    pub fn list(&mut self, dir: &[u8], count: u32, plus: bool) -> Res<Vec<String>> {
        let (mut cookie, mut verf, mut names) = (0, 0, Vec::new());
        loop {
            let (page, v, eof) = self.readdir_page(dir, cookie, verf, count, plus)?;
            verf = v;
            if let Some(last) = page.last() {
                cookie = last.2;
            }
            names.extend(page.into_iter().map(|e| e.0));
            if eof {
                return Ok(names);
            }
        }
    }

    pub fn fsinfo(&mut self, fh: &[u8]) -> Res<(u32, u32, u32)> {
        self.ok(19, &Self::fh_args(fh), |r| {
            post_attr(r);
            let rtmax = r.u32().unwrap();
            let _ = (r.u32(), r.u32());
            let wtmax = r.u32().unwrap();
            let _ = (r.u32(), r.u32(), r.u32(), r.u64(), r.u32(), r.u32());
            (rtmax, wtmax, r.u32().unwrap())
        })
    }

    pub fn fsstat(&mut self, fh: &[u8]) -> Res<u64> {
        self.ok(18, &Self::fh_args(fh), |r| {
            post_attr(r);
            let t = r.u64().unwrap();
            for _ in 0..5 {
                r.u64().unwrap();
            }
            r.u32().unwrap();
            t
        })
    }

    pub fn pathconf(&mut self, fh: &[u8]) -> Res<(u32, bool, bool)> {
        self.ok(20, &Self::fh_args(fh), |r| {
            post_attr(r);
            let _ = r.u32();
            let name_max = r.u32().unwrap();
            let _ = (r.bool(), r.bool());
            let ci = r.bool().unwrap();
            let cp = r.bool().unwrap();
            (name_max, ci, cp)
        })
    }

    pub fn commit(&mut self, fh: &[u8]) -> Res<Vec<u8>> {
        let mut w = Self::fh_args(fh);
        w.u64(0).u32(0);
        self.ok(21, &w, |r| {
            wcc(r);
            r.fixed(8).unwrap().to_vec()
        })
    }
}
