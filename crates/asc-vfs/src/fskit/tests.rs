//! The FSKit server over its socket, against the fixture trees. No mount is needed: the tests
//! are the client, speaking the protocol the Swift module speaks.

use std::io::BufReader;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;

use asc_core::path::RepoPath;
use asc_store::bucket::BucketObjects;
use asc_store::cachedir::CacheDir;
use asc_wc::journal::Op;
use asc_wc::redirect::Redirections;
use asc_wc::vstatus::PathState;

use super::wire::{self, err, kind, op, set};
use super::{FskitProvider, find_group, socket_path, volume_url};
use crate::core::{CoreFs, Options, Provider};
use crate::nfs::xdr::{Reader, Writer};
use crate::policy::HydrationPolicy;
use crate::unixfs::Config;
use crate::unixfs::inodes::{MemInodes, ROOT};
use crate::unixfs::testkit::*;

struct Fx {
    provider: Option<FskitProvider>,
    core: Arc<CoreFs>,
    journal: Arc<Recorded>,
    inodes: Arc<MemInodes>,
    tmp: tempfile::TempDir,
    rt: tokio::runtime::Runtime,
    sock: PathBuf,
}

/// An item's attributes as the wire carries them.
#[derive(Clone, Debug, PartialEq, Eq)]
struct A {
    ino: u64,
    parent: u64,
    kind: u32,
    perm: u32,
    uid: u32,
    size: u64,
}

fn attr(r: &mut Reader<'_>) -> A {
    let a = A {
        ino: r.u64().unwrap(),
        parent: r.u64().unwrap(),
        kind: r.u32().unwrap(),
        perm: r.u32().unwrap(),
        uid: {
            let _nlink = r.u32().unwrap();
            r.u32().unwrap()
        },
        size: {
            let _gid = r.u32().unwrap();
            r.u64().unwrap()
        },
    };
    let _alloc = r.u64().unwrap();
    for _ in 0..3 {
        wire::get_time(r).unwrap();
    }
    a
}

impl Fx {
    fn new() -> Self {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let store = Arc::new(BucketObjects::memory());
        let (tree1, _) = rt.block_on(build(&store));
        let core = Arc::new(CoreFs::new(
            store,
            tree1,
            Options {
                redirections: Redirections::parse("node_modules\n"),
                policy: HydrationPolicy::allow_all(),
                ..Options::default()
            },
        ));
        let mut fx = Self {
            provider: None,
            core,
            journal: Arc::new(Recorded::default()),
            inodes: Arc::new(MemInodes::default()),
            tmp: tempfile::tempdir().unwrap(),
            rt,
            sock: PathBuf::new(),
        };
        fx.serve();
        fx
    }

    fn config(&self) -> Config {
        Config {
            core: self.core.clone(),
            local: Some(
                CacheDir::open(self.tmp.path().join("cache"), self.tmp.path().join("ctmp"))
                    .unwrap(),
            ),
            inodes: self.inodes.clone(),
            overlay: self.tmp.path().join("overlay"),
            redirect: self.tmp.path().join("redirect"),
            redirections: Redirections::parse("node_modules\n"),
            times: Arc::new(FixedTimes),
            journal: self.journal.clone(),
            rt: self.rt.handle().clone(),
            root_time: EPOCH_TIME,
            gitfile: None,
        }
    }

    fn serve(&mut self) {
        let p = FskitProvider::new(self.config(), self.tmp.path().join("g")).unwrap();
        self.sock = p.serve().unwrap();
        self.provider = Some(p);
    }

    fn restart(&mut self) {
        self.provider = None;
        self.serve();
    }

    fn provider(&self) -> &FskitProvider {
        self.provider.as_ref().unwrap()
    }

    fn client(&self) -> Client {
        Client::connect(&self.sock)
    }

    fn state(&self, p: &str) -> PathState {
        self.provider()
            .disk_state()
            .state(&RepoPath::new(p).unwrap())
            .unwrap()
    }
}

struct Client {
    out: UnixStream,
    inp: BufReader<UnixStream>,
    xid: u32,
}

type R<T> = Result<T, u32>;

impl Client {
    fn connect(p: &std::path::Path) -> Self {
        let s = UnixStream::connect(p).unwrap();
        Self {
            inp: BufReader::new(s.try_clone().unwrap()),
            out: s,
            xid: 0,
        }
    }

    fn call(&mut self, code: u32, args: impl FnOnce(&mut Writer)) -> R<Vec<u8>> {
        self.xid += 1;
        let mut w = Writer::new();
        w.u32(self.xid).u32(code);
        args(&mut w);
        wire::write_frame(&mut self.out, w.bytes()).unwrap();
        let reply = wire::read_frame(&mut self.inp).unwrap().unwrap();
        let mut r = Reader::new(&reply);
        assert_eq!(r.u32().unwrap(), self.xid);
        match r.u32().unwrap() {
            0 => Ok(reply[8..].to_vec()),
            e => Err(e),
        }
    }

    fn hello(&mut self, v: u32) -> R<(u64, A, u32)> {
        let b = self.call(op::HELLO, |w| {
            w.u32(v);
        })?;
        let mut r = Reader::new(&b);
        assert_eq!(r.u32().unwrap(), wire::VERSION);
        let root = r.u64().unwrap();
        let a = attr(&mut r);
        Ok((root, a, r.u32().unwrap()))
    }

    fn one_attr(b: &[u8]) -> A {
        attr(&mut Reader::new(b))
    }

    fn getattr(&mut self, ino: u64) -> R<A> {
        self.call(op::GETATTR, |w| {
            w.u64(ino);
        })
        .map(|b| Self::one_attr(&b))
    }

    fn lookup(&mut self, dir: u64, name: &str) -> R<A> {
        self.call(op::LOOKUP, |w| {
            w.u64(dir).opaque(name.as_bytes());
        })
        .map(|b| Self::one_attr(&b))
    }

    fn walk(&mut self, path: &str) -> R<A> {
        let mut cur = ROOT;
        let mut a = None;
        for part in path.split('/') {
            let x = self.lookup(cur, part)?;
            cur = x.ino;
            a = Some(x);
        }
        Ok(a.unwrap())
    }

    fn forget(&mut self, ino: u64, n: u64) {
        self.call(op::FORGET, |w| {
            w.u64(ino).u64(n);
        })
        .unwrap();
    }

    fn read(&mut self, ino: u64, off: u64, n: u32) -> R<(Vec<u8>, bool)> {
        let b = self.call(op::READ, |w| {
            w.u64(ino).u64(off).u32(n);
        })?;
        let mut r = Reader::new(&b);
        let eof = r.bool().unwrap();
        Ok((r.opaque(1 << 21).unwrap().to_vec(), eof))
    }

    fn write(&mut self, ino: u64, off: u64, data: &[u8]) -> R<(u32, A)> {
        let b = self.call(op::WRITE, |w| {
            w.u64(ino).u64(off).opaque(data);
        })?;
        let mut r = Reader::new(&b);
        Ok((r.u32().unwrap(), attr(&mut r)))
    }

    fn make(&mut self, code: u32, dir: u64, name: &str, mode: u32) -> R<A> {
        self.call(code, |w| {
            w.u64(dir).opaque(name.as_bytes()).u32(mode);
        })
        .map(|b| Self::one_attr(&b))
    }

    fn symlink(&mut self, dir: u64, name: &str, target: &str) -> R<A> {
        self.call(op::SYMLINK, |w| {
            w.u64(dir).opaque(name.as_bytes()).opaque(target.as_bytes());
        })
        .map(|b| Self::one_attr(&b))
    }

    fn readlink(&mut self, ino: u64) -> R<Vec<u8>> {
        let b = self.call(op::READLINK, |w| {
            w.u64(ino);
        })?;
        Ok(Reader::new(&b).opaque(4096).unwrap().to_vec())
    }

    fn remove(&mut self, dir: u64, name: &str, is_dir: bool) -> R<()> {
        self.call(op::REMOVE, |w| {
            w.u64(dir).opaque(name.as_bytes()).bool(is_dir);
        })
        .map(drop)
    }

    fn rename(&mut self, from: u64, f: &str, to: u64, t: &str, flags: u32) -> R<()> {
        self.call(op::RENAME, |w| {
            w.u64(from)
                .opaque(f.as_bytes())
                .u64(to)
                .opaque(t.as_bytes())
                .u32(flags);
        })
        .map(drop)
    }

    fn setattr(&mut self, ino: u64, mask: u32, mode: u32, size: u64) -> R<A> {
        self.call(op::SETATTR, |w| {
            w.u64(ino).u32(mask).u32(mode).u32(0).u32(0).u64(size);
            let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
            wire::put_time(w, t);
            wire::put_time(w, t);
        })
        .map(|b| Self::one_attr(&b))
    }

    /// One page: `(verifier, [(name, ino, cookie, kind, attr)], eof)`.
    #[allow(clippy::type_complexity)]
    fn readdir(
        &mut self,
        dir: u64,
        cookie: u64,
        verf: u64,
        max: u32,
        attrs: bool,
    ) -> R<(u64, Vec<(String, u64, u64, u32, Option<A>)>, bool)> {
        let b = self.call(op::READDIR, |w| {
            w.u64(dir).u64(cookie).u64(verf).u32(max).bool(attrs);
        })?;
        let mut r = Reader::new(&b);
        let verf = r.u64().unwrap();
        let mut out = Vec::new();
        while r.bool().unwrap() {
            let ino = r.u64().unwrap();
            let name = String::from_utf8(r.opaque(1024).unwrap().to_vec()).unwrap();
            let cookie = r.u64().unwrap();
            let k = r.u32().unwrap();
            let a = r.bool().unwrap().then(|| attr(&mut r));
            out.push((name, ino, cookie, k, a));
        }
        Ok((verf, out, r.bool().unwrap()))
    }

    fn names(&mut self, dir: u64) -> Vec<String> {
        let (_, e, eof) = self.readdir(dir, 0, 0, 4096, false).unwrap();
        assert!(eof);
        let mut v: Vec<String> = e.into_iter().map(|e| e.0).collect();
        v.sort();
        v
    }
}

#[test]
fn hello_agrees_on_the_version_and_names_the_root() {
    let fx = Fx::new();
    let mut c = fx.client();
    let (root, a, max) = c.hello(wire::VERSION).unwrap();
    assert_eq!(
        (root, a.ino, a.kind, a.parent),
        (ROOT, ROOT, kind::DIR, ROOT)
    );
    assert_eq!(max, wire::MAX_IO);
    assert_eq!(c.hello(99).err(), Some(err::RPCMISMATCH));
    // Unknown operations and arguments that do not decode are answered, not dropped.
    assert_eq!(c.call(77, |_| {}), Err(err::NOSYS));
    assert_eq!(
        c.call(op::GETATTR, |w| {
            w.u32(1);
        }),
        Err(err::INVAL)
    );
    assert_eq!(c.getattr(ROOT).unwrap().kind, kind::DIR);
    assert_eq!(c.getattr(987_654_321), Err(err::STALE));
}

#[test]
fn browsing_does_not_hydrate_and_reads_do() {
    let fx = Fx::new();
    let mut c = fx.client();
    assert_eq!(c.names(ROOT), ["README", "link", "run.sh", "src"]);
    let (_, entries, _) = c.readdir(ROOT, 0, 0, 4096, true).unwrap();
    let readme = entries.iter().find(|e| e.0 == "README").unwrap();
    let ra = readme.4.clone().unwrap();
    assert_eq!(
        (ra.kind, ra.size, ra.perm & 0o111, ra.parent),
        (kind::FILE, 8, 0, ROOT)
    );
    assert_eq!(c.lookup(ROOT, "run.sh").unwrap().perm & 0o100, 0o100);
    let link = c.lookup(ROOT, "link").unwrap();
    assert_eq!(link.kind, kind::SYMLINK);
    assert_eq!(c.readlink(link.ino).unwrap(), b"README");
    assert_eq!(c.lookup(ROOT, "nope"), Err(err::NOENT));
    assert_eq!(c.lookup(ROOT, ".."), Err(err::INVAL));
    let src = c.lookup(ROOT, "src").unwrap();
    assert_eq!(c.names(src.ino), ["a.txt", "b.txt", "sub"]);
    assert_eq!(fx.state("README"), PathState::Virtual);

    let readme = c.lookup(ROOT, "README").unwrap();
    assert_eq!(readme.ino, ra.ino, "listing and lookup agree on the number");
    assert_eq!(
        c.read(readme.ino, 0, 4096).unwrap(),
        (b"read me\n".to_vec(), true)
    );
    assert_eq!(c.read(readme.ino, 5, 2).unwrap(), (b"me".to_vec(), false));
    assert_eq!(c.read(readme.ino, 100, 10).unwrap(), (vec![], true));
    let deep = c.walk("src/sub/deep.txt").unwrap();
    assert_eq!(deep.parent, c.lookup(src.ino, "sub").unwrap().ino);
    assert_eq!(c.read(deep.ino, 0, 100).unwrap().0, b"deep\n");
    assert!(fx.journal.ops().is_empty(), "reading journals nothing");
    assert_eq!(c.read(src.ino, 0, 1), Err(err::ISDIR));
    assert!(fx.provider().open_files() > 0);
    c.call(op::CLOSE, |w| {
        w.u64(readme.ino);
    })
    .unwrap();
    c.call(op::CLOSE, |w| {
        w.u64(deep.ino);
    })
    .unwrap();
    assert_eq!(fx.provider().open_files(), 0);
    let s = c.call(op::STATFS, |_| {}).unwrap();
    let mut r = Reader::new(&s);
    assert!(r.u64().unwrap() > 0);
}

#[test]
fn writes_creates_renames_and_removes_land_in_the_overlay() {
    let fx = Fx::new();
    let mut c = fx.client();
    let src = c.lookup(ROOT, "src").unwrap().ino;

    let a = c.lookup(src, "a.txt").unwrap().ino;
    let (n, after) = c.write(a, 0, b"ALPHA").unwrap();
    assert_eq!((n, after.size), (5, 6));
    assert_eq!(c.read(a, 0, 100).unwrap().0, b"ALPHA\n");
    c.call(op::SYNC, |w| {
        w.u64(a);
    })
    .unwrap();
    assert_eq!(fx.state("src/a.txt"), PathState::Full);

    let new = c.make(op::CREATE, src, "new.txt", 0o640).unwrap();
    assert_eq!(
        (new.kind, new.perm & 0o777, new.parent),
        (kind::FILE, 0o640, src)
    );
    c.write(new.ino, 0, b"fresh\n").unwrap();
    assert_eq!(c.make(op::CREATE, src, "new.txt", 0o640), Err(err::EXIST));
    let dir = c.make(op::MKDIR, ROOT, "made", 0o755).unwrap();
    assert_eq!(dir.kind, kind::DIR);
    let sl = c.symlink(dir.ino, "to-readme", "../README").unwrap();
    assert_eq!(c.readlink(sl.ino).unwrap(), b"../README");

    c.rename(src, "new.txt", dir.ino, "moved.txt", 0).unwrap();
    assert_eq!(c.lookup(src, "new.txt"), Err(err::NOENT));
    let moved = c.lookup(dir.ino, "moved.txt").unwrap();
    assert_eq!(moved.ino, new.ino, "a rename keeps the number");
    assert_eq!(
        c.rename(dir.ino, "moved.txt", src, "b.txt", wire::RENAME_NOREPLACE),
        Err(err::EXIST)
    );
    assert_eq!(
        c.rename(dir.ino, "moved.txt", src, "b.txt", 4),
        Err(err::NOTSUP)
    );
    c.rename(dir.ino, "moved.txt", src, "b.txt", 0).unwrap();
    let b = c.lookup(src, "b.txt").unwrap().ino;
    assert_eq!(c.read(b, 0, 100).unwrap().0, b"fresh\n");

    let t = c.setattr(a, set::SIZE | set::MODE, 0o600, 2).unwrap();
    assert_eq!((t.size, t.perm & 0o777), (2, 0o600));
    assert_eq!(c.read(a, 0, 100).unwrap().0, b"AL");

    c.remove(ROOT, "README", false).unwrap();
    assert_eq!(c.lookup(ROOT, "README"), Err(err::NOENT));
    assert_eq!(c.remove(ROOT, "made", true), Err(err::NOTEMPTY));
    c.remove(dir.ino, "to-readme", false).unwrap();
    c.remove(ROOT, "made", true).unwrap();
    assert_eq!(c.remove(ROOT, "src", false), Err(err::ISDIR));
    assert_eq!(c.remove(ROOT, "run.sh", true), Err(err::NOTDIR));
    assert_eq!(fx.state("README"), PathState::Tombstone);
    assert_eq!(
        c.make(op::CREATE, ROOT, &"x".repeat(300), 0o600),
        Err(err::NAMETOOLONG)
    );
    assert!(c.make(op::CREATE, ROOT, "a/b", 0o600).is_err());

    let ops = fx.journal.ops();
    for want in [
        (Op::Modified, "src/a.txt"),
        (Op::Created, "src/new.txt"),
        (Op::Created, "made"),
        (Op::Deleted, "README"),
    ] {
        assert!(
            ops.iter().any(|(o, p)| *o == want.0 && p == want.1),
            "{want:?} in {ops:?}"
        );
    }
}

#[test]
fn listings_page_by_cookie_and_a_lost_verifier_is_stale() {
    let fx = Fx::new();
    let mut c = fx.client();
    let (verf, first, eof) = c.readdir(ROOT, 0, 0, 2, false).unwrap();
    assert_eq!(first.len(), 2);
    assert!(!eof);
    let (v2, rest, eof) = c
        .readdir(ROOT, first.last().unwrap().2, verf, 100, false)
        .unwrap();
    assert_eq!(v2, verf);
    assert!(eof);
    let mut all: Vec<String> = first.iter().chain(&rest).map(|e| e.0.clone()).collect();
    all.sort();
    assert_eq!(all, ["README", "link", "run.sh", "src"]);
    assert_eq!(
        c.readdir(ROOT, 1, verf + 1000, 10, false).err(),
        Some(err::STALE)
    );
    let src = c.lookup(ROOT, "src").unwrap().ino;
    assert_eq!(
        c.readdir(src, 1, verf, 10, false).err(),
        Some(err::STALE),
        "another directory"
    );
    assert_eq!(c.readdir(ROOT, 99, verf, 10, false).err(), Some(err::INVAL));
    let file = c.lookup(ROOT, "README").unwrap().ino;
    assert_eq!(c.readdir(file, 0, 0, 10, false).err(), Some(err::NOTDIR));
}

#[test]
fn lookups_are_held_per_connection_until_forgotten_or_closed() {
    let fx = Fx::new();
    let mut c = fx.client();
    let readme = c.lookup(ROOT, "README").unwrap().ino;
    c.lookup(ROOT, "README").unwrap();
    let deep = c.walk("src/sub/deep.txt").unwrap().ino;
    assert_eq!(fx.provider().held(), 5);
    c.forget(readme, 1);
    assert_eq!(fx.provider().held(), 4);
    // Forgetting more than is held releases only what this connection holds.
    c.forget(readme, 10);
    c.forget(readme, 1);
    assert_eq!(fx.provider().held(), 3);
    let live = fx.provider().merged().table.live();
    assert!(live > 1);
    // A second connection holds its own.
    let mut d = fx.client();
    d.lookup(ROOT, "README").unwrap();
    assert_eq!(fx.provider().held(), 4);
    drop(d);
    // The close is seen by the reader thread; `held` counts open connections only, so wait for
    // the table to drop README again.
    let t0 = std::time::Instant::now();
    while fx.provider().merged().table.path_of(readme).is_some() {
        assert!(
            t0.elapsed().as_secs() < 10,
            "the closed connection's lookup was released"
        );
        std::thread::yield_now();
    }
    assert_eq!(
        fx.provider().merged().table.path_of(deep).unwrap(),
        b"src/sub/deep.txt"
    );
    drop(c);
    let t0 = std::time::Instant::now();
    while fx.provider().merged().table.live() > 1 {
        assert!(
            t0.elapsed().as_secs() < 10,
            "everything is released on close"
        );
        std::thread::yield_now();
    }
}

#[test]
fn an_item_kept_across_a_restart_is_revived() {
    let mut fx = Fx::new();
    let deep = {
        let mut c = fx.client();
        let deep = c.walk("src/sub/deep.txt").unwrap().ino;
        assert_eq!(c.read(deep, 0, 100).unwrap().0, b"deep\n");
        deep
    };
    fx.restart();
    let mut c = fx.client();
    // The client still has the item; the server makes its number live from the stored path.
    assert_eq!(c.getattr(deep).unwrap().size, 5);
    assert_eq!(c.read(deep, 0, 100).unwrap().0, b"deep\n");
    assert_eq!(
        fx.provider().held(),
        1,
        "the revival counts a lookup the client forgets"
    );
    c.forget(deep, 1);
    assert_eq!(fx.provider().held(), 0);
}

#[test]
fn the_socket_is_owner_only_and_refuses_a_second_server() {
    let fx = Fx::new();
    let dir = fx.sock.parent().unwrap();
    assert_eq!(std::fs::metadata(dir).unwrap().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(&fx.sock).unwrap().mode() & 0o777, 0o600);
    assert_eq!(
        fx.sock,
        socket_path(&fx.tmp.path().join("g"), {
            let name = fx.sock.file_stem().unwrap().to_str().unwrap();
            u64::from_str_radix(name, 16).unwrap()
        })
    );
    let url = fx.provider().url();
    assert!(url.starts_with("ascfs://") && url.len() == 8 + 16, "{url}");
    assert_eq!(volume_url(255), "ascfs://00000000000000ff");
    // A second server over the same work tree finds the first one listening.
    let other = FskitProvider::new(fx.config(), fx.tmp.path().join("g")).unwrap();
    assert_eq!(other.socket(), fx.sock, "the id is kept in the overlay");
    let e = other.serve().unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::AddrInUse);
}

#[test]
fn a_stale_socket_is_replaced_and_a_linked_directory_refused() {
    let mut fx = Fx::new();
    let sock = fx.sock.clone();
    // A crashed server leaves its socket behind: bound, with nobody listening.
    fx.provider = None;
    drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
    assert!(sock.exists());
    fx.serve();
    fx.client().hello(wire::VERSION).unwrap();
    // A shared directory is made private again.
    let dir = sock.parent().unwrap().to_path_buf();
    fx.provider = None;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    fx.serve();
    assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
    // A link where the directory should be is refused.
    fx.provider = None;
    let elsewhere = fx.tmp.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &dir).unwrap();
    let p = FskitProvider::new(fx.config(), fx.tmp.path().join("g")).unwrap();
    assert!(p.serve().is_err());
}

#[test]
fn the_group_directory_is_found_by_its_suffix() {
    let t = tempfile::tempdir().unwrap();
    assert_eq!(find_group(t.path()), None);
    std::fs::create_dir(t.path().join("ABCDE12345.dev.asc.fskit")).unwrap();
    std::fs::create_dir(t.path().join("com.example.other")).unwrap();
    std::fs::write(t.path().join("X.dev.asc.fskit"), b"a file").unwrap();
    assert_eq!(
        find_group(t.path()),
        Some(t.path().join("ABCDE12345.dev.asc.fskit"))
    );
    // Two candidates: neither is guessed.
    std::fs::create_dir(t.path().join("FGHIJ67890.dev.asc.fskit")).unwrap();
    assert_eq!(find_group(t.path()), None);
    // A bare suffix is no group.
    let u = tempfile::tempdir().unwrap();
    std::fs::create_dir(u.path().join(".dev.asc.fskit")).unwrap();
    assert_eq!(find_group(u.path()), None);
}

#[test]
fn a_socket_path_too_long_for_macos_is_refused_up_front() {
    let fx = Fx::new();
    let long = fx.tmp.path().join("g".repeat(100));
    let Err(e) = FskitProvider::new(fx.config(), long) else {
        panic!("a long path is refused");
    };
    assert!(e.to_string().contains("ASC_FSKIT_GROUP_DIR"), "{e}");
}
