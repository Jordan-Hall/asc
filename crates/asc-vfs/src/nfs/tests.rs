//! The NFS server over the wire, against the fixture trees. No mount is needed: the tests are
//! the client.

use std::net::SocketAddr;
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;

use asc_core::id::{FileId, GitOid, TreeId};
use asc_core::object::{Change, Kind};
use asc_core::path::RepoPath;
use asc_store::bucket::BucketObjects;
use asc_store::cachedir::CacheDir;
use asc_wc::journal::Op;
use asc_wc::redirect::Redirections;
use asc_wc::switch::Plan;
use asc_wc::times::Switched;
use asc_wc::vstatus::PathState;

use super::NfsProvider;
use super::client::Client;
use super::server::{MOUNT_PROG, NFS_PROG, stat};
use crate::core::{CoreFs, Options, Provider};
use crate::policy::HydrationPolicy;
use crate::unixfs::Config;
use crate::unixfs::inodes::MemInodes;
use crate::unixfs::testkit::*;

const NF3REG: u32 = 1;
const NF3DIR: u32 = 2;
const NF3LNK: u32 = 5;

struct Nx {
    provider: Option<NfsProvider>,
    core: Arc<CoreFs>,
    journal: Arc<Recorded>,
    inodes: Arc<MemInodes>,
    store: Arc<BucketObjects>,
    tree2: TreeId,
    tmp: tempfile::TempDir,
    rt: tokio::runtime::Runtime,
    addr: SocketAddr,
    uid: u32,
}

fn rp(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap()
}

impl Nx {
    fn new(deny: &str) -> Self {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let store = Arc::new(BucketObjects::memory());
        let (tree1, tree2) = rt.block_on(build(&store));
        let policy = if deny.is_empty() {
            HydrationPolicy::allow_all()
        } else {
            HydrationPolicy::parse(deny)
        };
        let core = Arc::new(CoreFs::new(
            store.clone(),
            tree1,
            Options {
                redirections: Redirections::parse("node_modules\n"),
                policy,
                ..Options::default()
            },
        ));
        let tmp = tempfile::tempdir().unwrap();
        let uid = std::fs::metadata(tmp.path()).unwrap().uid();
        let mut nx = Self {
            provider: None,
            core,
            journal: Arc::new(Recorded::default()),
            inodes: Arc::new(MemInodes::default()),
            store,
            tree2,
            tmp,
            rt,
            addr: "127.0.0.1:0".parse().unwrap(),
            uid,
        };
        nx.serve();
        nx
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
        }
    }

    fn serve(&mut self) {
        let p = NfsProvider::new(self.config()).unwrap().allow_any_port();
        self.addr = p.serve().unwrap();
        self.provider = Some(p);
    }

    /// Stops the server and starts a new one over the same overlay and inode table.
    fn restart(&mut self) {
        self.provider = None;
        self.serve();
    }

    fn provider(&self) -> &NfsProvider {
        self.provider.as_ref().unwrap()
    }

    fn client(&self) -> Client {
        Client::connect(self.addr, self.uid, self.uid)
    }

    fn state(&self, p: &str) -> PathState {
        self.provider().disk_state().state(&rp(p)).unwrap()
    }
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

#[test]
fn mount_protocol_exports_and_refuses_strangers() {
    let nx = Nx::new("");
    let mut c = nx.client();
    // NULL needs no credential.
    c.cred = None;
    assert_eq!(c.call(NFS_PROG, 3, 0, &[]), Ok(vec![]));
    assert_eq!(c.call(MOUNT_PROG, 3, 0, &[]), Ok(vec![]));
    // Anything else needs AUTH_SYS.
    assert_eq!(c.call(NFS_PROG, 3, 1, &[0, 0, 0, 0]), Err((1, 105)));
    c.cred = Some((nx.uid, nx.uid));
    // Wrong versions, programs and procedures.
    assert_eq!(c.call(NFS_PROG, 4, 0, &[]), Err((0, 2)));
    assert_eq!(c.call(100_021, 1, 0, &[]), Err((0, 1)));
    assert_eq!(c.call(NFS_PROG, 3, 99, &[]), Err((0, 3)));
    assert_eq!(c.call(NFS_PROG, 3, 1, &[1, 2]), Err((0, 4)));
    // Only the export mounts.
    assert_eq!(c.mnt("/elsewhere"), Err(2));
    let root = c.root();
    assert_eq!(root.len(), 16);
    assert_eq!(c.getattr(&root).unwrap().kind, NF3DIR);
    // Another user is refused, both at mount and per call.
    let mut other = Client::connect(nx.addr, nx.uid.wrapping_add(4242), 0);
    if nx.uid != 0 {
        assert_eq!(other.mnt("/asc"), Err(13));
    }
    assert_eq!(other.getattr(&root), Err(stat::ACCES));
}

#[test]
fn browsing_does_not_hydrate_and_reads_do() {
    let nx = Nx::new("");
    let mut c = nx.client();
    let root = c.root();
    assert_eq!(
        sorted(c.list(&root, 4096, false).unwrap()),
        [".", "..", "README", "link", "run.sh", "src"]
    );
    let plus = c.readdir_page(&root, 0, 0, 65536, true).unwrap();
    let readme = plus.0.iter().find(|e| e.0 == "README").unwrap();
    let (attr, fh) = readme.3.clone().unwrap();
    assert_eq!((attr.kind, attr.size, attr.mode & 0o111), (NF3REG, 8, 0));
    let (_, run) = c.lookup(&root, "run.sh").unwrap();
    assert_eq!(run.mode & 0o100, 0o100);
    let (link, la) = c.lookup(&root, "link").unwrap();
    assert_eq!(la.kind, NF3LNK);
    assert_eq!(c.readlink(&link).unwrap(), b"README");
    assert_eq!(c.lookup(&root, "nope"), Err(stat::NOENT));
    let (src, _) = c.lookup(&root, "src").unwrap();
    assert_eq!(
        sorted(c.list(&src, 4096, true).unwrap()),
        [".", "..", "a.txt", "b.txt", "sub"]
    );
    assert_eq!(nx.state("README"), PathState::Virtual);
    assert_eq!(nx.state("src/a.txt"), PathState::Virtual);

    assert_eq!(c.read(&fh, 0, 4096).unwrap(), (b"read me\n".to_vec(), true));
    assert_eq!(c.read(&fh, 5, 2).unwrap(), (b"me".to_vec(), false));
    assert_eq!(c.read(&fh, 100, 10).unwrap(), (vec![], true));
    let (deep, _) = c.walk("src/sub/deep.txt").unwrap();
    assert_eq!(c.read(&deep, 0, 100).unwrap().0, b"deep\n");
    // Reading changes nothing a status would see, and nothing is journaled.
    assert_ne!(nx.state("src/sub/deep.txt"), PathState::Missing);
    assert!(nx.journal.ops().is_empty());
    assert_eq!(c.read(&src, 0, 1), Err(stat::ISDIR));
    assert_eq!(c.access(&fh, 0x3f).unwrap() & 0x1, 0x1);
}

#[test]
fn file_system_information_matches_the_mount_options() {
    let nx = Nx::new("");
    let mut c = nx.client();
    let root = c.root();
    let (rtmax, wtmax, props) = c.fsinfo(&root).unwrap();
    assert_eq!((rtmax, wtmax), (1 << 20, 1 << 20));
    assert_eq!(props & 0x1b, 0x1a);
    assert!(c.fsstat(&root).unwrap() > 0);
    assert_eq!(c.pathconf(&root).unwrap(), (255, false, true));
}

#[test]
fn writes_creates_renames_and_removes_land_in_the_overlay() {
    let nx = Nx::new("");
    let mut c = nx.client();
    let root = c.root();
    let (src, _) = c.lookup(&root, "src").unwrap();

    // Edit a tracked file in place.
    let (a, _) = c.lookup(&src, "a.txt").unwrap();
    let (n, committed, verf) = c.write(&a, 0, b"ALPHA", 0).unwrap();
    assert_eq!((n, committed), (5, 0));
    assert_eq!(c.read(&a, 0, 100).unwrap().0, b"ALPHA\n");
    assert_eq!(c.commit(&a).unwrap(), verf);
    let (_, committed, _) = c.write(&a, 6, b"more\n", 2).unwrap();
    assert_eq!(committed, 2);
    assert_eq!(c.getattr(&a).unwrap().size, 11);
    assert_eq!(nx.state("src/a.txt"), PathState::Full);

    // New files and directories.
    let (new, na) = c.create(&src, "new.txt", 1, 0o640, [0; 8]).unwrap();
    assert_eq!((na.kind, na.mode & 0o777, na.uid), (NF3REG, 0o640, nx.uid));
    c.write(&new, 0, b"fresh\n", 1).unwrap();
    assert_eq!(
        c.create(&src, "new.txt", 1, 0o640, [0; 8]),
        Err(stat::EXIST)
    );
    // Unchecked over an existing file keeps its data.
    c.create(&src, "new.txt", 0, 0o600, [0; 8]).unwrap();
    assert_eq!(c.read(&new, 0, 100).unwrap().0, b"fresh\n");
    let (dir, da) = c.mkdir(&root, "made").unwrap();
    assert_eq!(da.kind, NF3DIR);
    assert_eq!(c.mkdir(&root, "made"), Err(stat::EXIST));
    let (sl, _) = c.symlink(&dir, "to-readme", "../README").unwrap();
    assert_eq!(c.readlink(&sl).unwrap(), b"../README");

    // Rename across directories, then over an existing name.
    c.rename(&src, "new.txt", &dir, "moved.txt").unwrap();
    assert_eq!(c.lookup(&src, "new.txt"), Err(stat::NOENT));
    let (moved, _) = c.lookup(&dir, "moved.txt").unwrap();
    assert_eq!(c.read(&moved, 0, 100).unwrap().0, b"fresh\n");
    c.rename(&dir, "moved.txt", &src, "b.txt").unwrap();
    let (b, _) = c.lookup(&src, "b.txt").unwrap();
    assert_eq!(c.read(&b, 0, 100).unwrap().0, b"fresh\n");

    // Removing a tracked file leaves a whiteout; directories go only when empty.
    c.remove(&root, "README").unwrap();
    assert_eq!(c.lookup(&root, "README"), Err(stat::NOENT));
    assert_eq!(c.remove(&root, "README"), Err(stat::NOENT));
    assert_eq!(c.rmdir(&root, "made"), Err(stat::NOTEMPTY));
    c.remove(&dir, "to-readme").unwrap();
    c.rmdir(&root, "made").unwrap();
    assert_eq!(c.remove(&root, "src"), Err(stat::ISDIR));
    assert_eq!(c.rmdir(&root, "run.sh"), Err(stat::NOTDIR));
    assert!(nx.provider().whiteouts().unwrap().contains(&rp("README")));
    assert_eq!(nx.state("README"), PathState::Tombstone);
    assert!(nx.provider().overlay_bytes().unwrap() > 0);

    let ops = nx.journal.ops();
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
    // Names the overlay cannot hold are refused.
    assert!(c.create(&root, "a/b", 1, 0o600, [0; 8]).is_err());
    assert!(c.create(&root, "..", 1, 0o600, [0; 8]).is_err());
    let long = "x".repeat(300);
    assert_eq!(
        c.create(&root, &long, 1, 0o600, [0; 8]),
        Err(stat::NAMETOOLONG)
    );
}

#[test]
fn exclusive_create_answers_a_retransmit_and_refuses_a_rival() {
    let nx = Nx::new("");
    let mut c = nx.client();
    let root = c.root();
    let verf = *b"verifier";
    let (fh, _) = c.create(&root, "lock", 2, 0, verf).unwrap();
    let (again, _) = c.create(&root, "lock", 2, 0, verf).unwrap();
    assert_eq!(fh, again);
    assert_eq!(
        c.create(&root, "lock", 2, 0, *b"another!"),
        Err(stat::EXIST)
    );
    // The client then sets the real attributes.
    let a = c.setattr(&fh, Some(0o644), None, None).unwrap().unwrap();
    assert_eq!(a.mode & 0o777, 0o644);
}

#[test]
fn setattr_truncates_and_honours_the_guard() {
    let nx = Nx::new("");
    let mut c = nx.client();
    let (a, before) = c.walk("src/a.txt").unwrap();
    let after = c.setattr(&a, None, Some(2), None).unwrap().unwrap();
    assert_eq!(after.size, 2);
    assert_eq!(c.read(&a, 0, 100).unwrap().0, b"al");
    // A stale guard is refused; the current one passes.
    assert_eq!(
        c.setattr(&a, Some(0o600), None, Some(before.ctime)),
        Err(stat::NOT_SYNC)
    );
    let now = c.getattr(&a).unwrap();
    c.setattr(&a, Some(0o600), None, Some(now.ctime)).unwrap();
    assert_eq!(c.getattr(&a).unwrap().mode & 0o777, 0o600);
    // Growing pads with zeros.
    c.setattr(&a, None, Some(4), None).unwrap();
    assert_eq!(c.read(&a, 0, 100).unwrap().0, b"al\0\0");
}

#[test]
fn bad_and_stale_handles_are_told_apart() {
    let nx = Nx::new("");
    let mut c = nx.client();
    let root = c.root();
    assert_eq!(c.getattr(&root[..8]), Err(stat::BADHANDLE));
    let mut forged = root.clone();
    forged[15] ^= 1;
    assert_eq!(c.getattr(&forged), Err(stat::STALE));
    let mut unknown = root.clone();
    unknown[..8].copy_from_slice(&u64::MAX.to_be_bytes());
    assert_eq!(c.getattr(&unknown), Err(stat::STALE));
    // A removed file's handle goes stale.
    let (b, _) = c.walk("src/b.txt").unwrap();
    let (src, _) = c.walk("src").unwrap();
    c.remove(&src, "b.txt").unwrap();
    assert_eq!(c.getattr(&b), Err(stat::STALE));
}

#[test]
fn handles_survive_a_server_restart() {
    let mut nx = Nx::new("");
    let mut c = nx.client();
    let (deep, attr) = c.walk("src/sub/deep.txt").unwrap();
    let (a, _) = c.walk("src/a.txt").unwrap();
    c.write(&a, 0, b"kept", 2).unwrap();
    assert!(nx.provider().held() >= 4);
    drop(c);
    nx.restart();
    assert_eq!(nx.provider().held(), 0);
    let mut c = nx.client();
    // The new server has never looked these up: it revives them from the inode table.
    assert_eq!(c.getattr(&deep).unwrap().fileid, attr.fileid);
    assert_eq!(c.read(&deep, 0, 100).unwrap().0, b"deep\n");
    assert_eq!(c.read(&a, 0, 4).unwrap().0, b"kept");
    assert!(nx.provider().held() >= 2);
}

#[test]
fn listings_page_by_cookie_and_refuse_stale_cookies() {
    let nx = Nx::new("");
    let mut c = nx.client();
    let root = c.root();
    // Small pages still see every name once.
    for plus in [false, true] {
        let names = c.list(&root, if plus { 400 } else { 160 }, plus).unwrap();
        assert_eq!(
            sorted(names),
            [".", "..", "README", "link", "run.sh", "src"],
            "plus {plus}"
        );
    }
    let (page, verf, eof) = c.readdir_page(&root, 0, 0, 160, false).unwrap();
    assert!(!eof && !page.is_empty());
    let cookie = page.last().unwrap().2;
    assert!(c.readdir_page(&root, cookie, verf, 4096, false).is_ok());
    assert_eq!(
        c.readdir_page(&root, cookie, verf ^ 0x55, 4096, false)
            .map(|_| ()),
        Err(stat::BAD_COOKIE)
    );
    assert_eq!(
        c.readdir_page(&root, 0, 0, 8, false).map(|_| ()),
        Err(stat::TOOSMALL)
    );
    let (file, _) = c.lookup(&root, "README").unwrap();
    assert_eq!(
        c.readdir_page(&file, 0, 0, 4096, false).map(|_| ()),
        Err(stat::NOTDIR)
    );
}

#[test]
fn a_switch_moves_handles_listings_and_directory_times() {
    let nx = Nx::new("");
    let mut c = nx.client();
    let (src, src_before) = c.walk("src").unwrap();
    let (a, a_before) = c.walk("src/a.txt").unwrap();
    let (b, _) = c.walk("src/b.txt").unwrap();
    let (deep, _) = c.walk("src/sub/deep.txt").unwrap();
    assert_eq!(c.read(&a, 0, 100).unwrap().0, b"alpha\n");
    assert_eq!(c.read(&deep, 0, 100).unwrap().0, b"deep\n");
    let (_, verf, _) = c.readdir_page(&src, 0, 0, 4096, false).unwrap();

    let write = |data: &[u8]| Change::Write {
        kind: Kind::File,
        file: FileId::of(data),
        size: data.len() as u64,
        git: GitOid::from_bytes([0; 20]),
        copy_from: None,
    };
    let plan = Plan {
        updates: vec![
            (rp("src/a.txt"), write(b"alpha two, longer\n")),
            (rp("src/sub/deep.txt"), write(b"deep two\n")),
            (rp("src/c.txt"), write(b"gamma\n")),
        ],
        deletes: vec![rp("src/b.txt")],
        ..Plan::default()
    };
    nx.core.set_root(nx.tree2);
    let out = nx
        .provider()
        .apply_switch(
            &plan,
            Switched {
                epoch: 2,
                time: EPOCH_TIME + 10,
            },
        )
        .unwrap();
    assert!(out.pending.is_empty());

    let a_after = c.getattr(&a).unwrap();
    assert_eq!(a_after.fileid, a_before.fileid);
    assert_eq!(a_after.size, 18);
    assert_eq!(c.read(&a, 0, 100).unwrap().0, b"alpha two, longer\n");
    assert_eq!(c.read(&deep, 0, 100).unwrap().0, b"deep two\n");
    assert_eq!(c.getattr(&b), Err(stat::STALE));
    let src_after = c.getattr(&src).unwrap();
    assert_ne!(
        (src_after.mtime, src_after.ctime),
        (src_before.mtime, src_before.ctime),
        "a cached listing must be dropped"
    );
    assert_eq!(
        sorted(c.list(&src, 4096, true).unwrap()),
        [".", "..", "a.txt", "c.txt", "sub"]
    );
    // A cookie from the old listing is refused.
    assert_eq!(
        c.readdir_page(&src, 2, verf, 4096, false).map(|_| ()),
        Err(stat::BAD_COOKIE)
    );
    assert_eq!(nx.state("src/sub/deep.txt"), PathState::Virtual);
}

#[test]
fn a_retransmitted_remove_gets_the_first_answer() {
    let nx = Nx::new("");
    let mut c = nx.client();
    let root = c.root();
    let mut args = super::xdr::Writer::new();
    args.opaque(&root).opaque(b"README");
    c.send(77, NFS_PROG, 3, 12, args.bytes());
    let (xid, first) = c.recv();
    assert_eq!(xid, 77);
    c.send(77, NFS_PROG, 3, 12, args.bytes());
    let (xid, second) = c.recv();
    assert_eq!(xid, 77);
    assert_eq!(first, second);
    assert_eq!(&first.unwrap()[..4], &[0, 0, 0, 0]);
    // A new transaction sees the file gone.
    assert_eq!(c.remove(&root, "README"), Err(stat::NOENT));
}

#[test]
fn the_deny_policy_applies_to_nfs_reads() {
    let nx = Nx::new("unknown");
    let mut c = nx.client();
    let (fh, attr) = c.walk("src/b.txt").unwrap();
    // Browsing works; reading content that is not local is refused.
    assert_eq!(attr.size, 5);
    assert!(c.read(&fh, 0, 100).is_err());
    assert_eq!(nx.state("src/b.txt"), PathState::Virtual);
}

#[test]
fn many_clients_read_at_once() {
    let nx = Nx::new("");
    let addr = nx.addr;
    let uid = nx.uid;
    let threads: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(move || {
                let mut c = Client::connect(addr, uid, uid);
                for _ in 0..20 {
                    let (fh, _) = c.walk("src/sub/deep.txt").unwrap();
                    assert_eq!(c.read(&fh, 0, 100).unwrap().0, b"deep\n");
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let _ = &nx.store;
}

#[test]
fn mount_answers_only_while_the_mount_command_runs() {
    let nx = Nx::new("");
    let mut c = nx.client();
    nx.provider().set_mounts_open(false);
    assert_eq!(
        c.mnt("/asc"),
        Err(13),
        "a shut MOUNT hands out no root handle"
    );
    nx.provider().set_mounts_open(true);
    assert!(c.mnt("/asc").is_ok());
}

#[test]
fn the_socket_table_names_the_owner_of_a_loopback_peer() {
    let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:03FF 0100007F:1F90 01 00000000:00000000 00:00000000 00000000     0        0 1 1
   1: 0100007F:D431 0100007F:1F90 01 00000000:00000000 00:00000000 00000000  1001        0 2 1
";
    assert_eq!(super::server::socket_uid(table, 0x03FF, 0x1F90), Some(0));
    assert_eq!(super::server::socket_uid(table, 0xD431, 0x1F90), Some(1001));
    assert_eq!(super::server::socket_uid(table, 0xD431, 0x1F91), None);
}

#[cfg(target_os = "linux")]
#[test]
fn only_the_kernel_and_the_owner_may_connect() {
    use std::os::unix::fs::MetadataExt;
    let me = std::fs::metadata("/proc/self").unwrap().uid();
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let _c = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let (s, _) = l.accept().unwrap();
    assert!(super::server::peer_allowed(&s, me));
    if me != 0 {
        assert!(
            !super::server::peer_allowed(&s, me + 1),
            "a stranger's socket is refused"
        );
    }
}
