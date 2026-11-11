//! A real Cloud Files sync root over a tree built in memory, and the tests that drive it the way
//! users and tools do.
//!
//! The filter sends no callbacks for I/O by the process that connected the sync root, so a
//! listing or a read from this process would see only what is already on disk. Every test
//! therefore does its work-tree I/O through [`Lab::other`]: this test binary run again as a child
//! process (the ignored `child` test), which the filter treats as any user's tool. It also works
//! on paths longer than `MAX_PATH`, which `cmd` does not.

use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use asc_core::id::TreeId;
use asc_core::path::RepoPath;
use asc_wc::journal::Op;
use asc_wc::switch::Mode;
use asc_wc::times::Switched;
use asc_wc::vstatus::PathState;

use super::provider::CloudFilesProvider;
use super::switch::{Report, Request};
use crate::core::{CoreFs, Options, Provider, Result};
use crate::policy::HydrationPolicy;
use crate::win::testkit::{Files, Holder, Kit, WORKSPACE, p, tempdir};
use crate::win::verbatim;

/// The child's operation: fields separated by newlines, the first naming it.
const OP_VAR: &str = "ASC_LAB_OP";
/// Where the child writes its result; an error goes to the same path with `.err` appended.
const OUT_VAR: &str = "ASC_LAB_OUT";

/// A mounted sync root over the kit's tree; the kit is reached through `Deref`.
pub(crate) struct Lab {
    kit: Kit,
    pub(crate) core: Arc<CoreFs>,
    pub(crate) provider: CloudFilesProvider,
    locked: Option<&'static str>,
}

impl Deref for Lab {
    type Target = Kit;

    fn deref(&self) -> &Kit {
        &self.kit
    }
}

impl Lab {
    /// Mounts a workspace with `files`, or `None` (saying why) where Cloud Files is unavailable.
    pub(crate) fn new(files: Files<'_>) -> Option<Self> {
        Self::build(files, "", None)
    }

    /// As `new`, with a hydration deny list.
    pub(crate) fn deny(files: Files<'_>, deny: &str) -> Option<Self> {
        Self::build(files, deny, None)
    }

    /// As `new`, with every path under `prefix` locked by the path rules.
    pub(crate) fn locked(files: Files<'_>, prefix: &'static str) -> Option<Self> {
        Self::build(files, "", Some(prefix))
    }

    fn make_core(kit: &Kit, root: TreeId, locked: Option<&'static str>) -> Arc<CoreFs> {
        let Some(prefix) = locked else {
            return kit.core(root);
        };
        let opts = Options {
            redirections: asc_wc::redirect::Redirections::parse("node_modules\n"),
            policy: HydrationPolicy::allow_all(),
            locked: Some(Arc::new(move |path: &RepoPath| {
                path.as_bytes().starts_with(prefix.as_bytes())
            })),
            ..Options::windows()
        };
        Arc::new(CoreFs::new(kit.store.clone(), root, opts))
    }

    fn build(files: Files<'_>, deny: &str, locked: Option<&'static str>) -> Option<Self> {
        if let Err(e) = super::ffi::platform_available() {
            eprintln!("skipping: {e}");
            return None;
        }
        let kit = Kit::new(deny);
        let root = kit.tree(files);
        let core = Self::make_core(&kit, root, locked);
        let provider = CloudFilesProvider::new(kit.config(WORKSPACE, core.clone())).unwrap();
        let lab = Self {
            kit,
            core,
            provider,
            locked,
        };
        lab.provider.mount(&lab.root()).unwrap();
        lab.provider.set_store(lab.store_handle());
        Some(lab)
    }

    /// The daemon starting again after `unmount`: a new provider connects over the same root,
    /// with the base tree `base`.
    pub(crate) fn restart(&mut self, base: TreeId) {
        self.core = Self::make_core(&self.kit, base, self.locked);
        self.provider =
            CloudFilesProvider::new(self.kit.config(WORKSPACE, self.core.clone())).unwrap();
        self.provider.mount(&self.root()).unwrap();
        self.provider.set_store(self.store_handle());
    }

    /// A switch from `old` to `new` at time `t`, stamping every changed path as the daemon does.
    pub(crate) fn run(
        &self,
        old: Files<'_>,
        new: Files<'_>,
        mode: Mode,
        extra: &[&str],
        t: i64,
    ) -> Result<Report> {
        let (plan, extra) = self.stamp(old, new, extra, t);
        let (from, to) = (self.tree(old), self.tree(new));
        self.provider.switch(
            &Request {
                mode,
                plan: &plan,
                extra: &extra,
                from_root: from,
                to_root: to,
                to_head: "refs/heads/master",
            },
            Switched { epoch: 2, time: t },
        )
    }

    /// The absolute verbatim path of `rel` (`""` is the root).
    pub(crate) fn abs(&self, rel: &str) -> PathBuf {
        if rel.is_empty() {
            verbatim(&self.root())
        } else {
            verbatim(&self.at(rel))
        }
    }

    /// Runs `op` on `args` (paths relative to the root, then data) in another process.
    pub(crate) fn other(&self, op: &str, paths: &[&str], data: &[u8]) -> io::Result<Vec<u8>> {
        let out_dir = tempdir();
        let out = out_dir.path().join("out");
        let mut spec = op.to_owned();
        for rel in paths {
            spec.push('\n');
            spec.push_str(&self.abs(rel).to_string_lossy());
        }
        if !data.is_empty() {
            let blob = out_dir.path().join("data");
            std::fs::write(&blob, data)?;
            spec.push('\n');
            spec.push_str(&blob.to_string_lossy());
        }
        let status = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "cloudfiles::lab::tests::child",
                "--ignored",
                "--test-threads=1",
                "-q",
            ])
            .env(OP_VAR, spec)
            .env(OUT_VAR, &out)
            .output()?
            .status;
        assert!(status.success(), "the child process failed: {status:?}");
        match std::fs::read(&out) {
            Ok(bytes) => Ok(bytes),
            Err(_) => Err(io::Error::other(
                std::fs::read_to_string(err_path(&out)).unwrap_or_default(),
            )),
        }
    }

    /// `rel` read by another process.
    pub(crate) fn read(&self, rel: &str) -> io::Result<Vec<u8>> {
        self.other("read", &[rel], b"")
    }

    /// The names in the directory `rel`, sorted, as another process lists them.
    pub(crate) fn list(&self, rel: &str) -> Vec<String> {
        let out = self.other("list", &[rel], b"").unwrap();
        let s = String::from_utf8(out).unwrap();
        s.lines().map(str::to_owned).collect()
    }

    pub(crate) fn write(&self, rel: &str, data: &[u8]) {
        self.other("write", &[rel], data).unwrap();
    }

    pub(crate) fn remove(&self, rel: &str) -> io::Result<()> {
        self.other("remove", &[rel], b"").map(drop)
    }

    pub(crate) fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.other("rename", &[from, to], b"").map(drop)
    }

    pub(crate) fn mkdir(&self, rel: &str) {
        self.other("mkdir", &[rel], b"").unwrap();
    }

    pub(crate) fn exists(&self, rel: &str) -> bool {
        self.other("exists", &[rel], b"").unwrap() == b"1"
    }

    /// Reads every file from another process, so each hydrates, checking its content.
    pub(crate) fn hydrate(&self, files: Files<'_>) {
        for (path, data) in files {
            assert_eq!(&self.read(path).unwrap(), data, "{path}");
        }
    }

    pub(crate) fn state(&self, path: &str) -> PathState {
        self.provider.disk_state().state(&p(path)).unwrap()
    }
}

fn err_path(out: &Path) -> PathBuf {
    let mut s = out.as_os_str().to_owned();
    s.push(".err");
    PathBuf::from(s)
}

/// The child side of [`Lab::other`].
fn child_op(spec: &str) -> io::Result<Vec<u8>> {
    let mut f = spec.split('\n');
    let op = f.next().unwrap_or_default();
    let a = PathBuf::from(f.next().unwrap_or_default());
    let b = f.next().map(PathBuf::from);
    match op {
        "read" => std::fs::read(&a),
        "list" => {
            let mut v = std::fs::read_dir(&a)?
                .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
                .collect::<io::Result<Vec<_>>>()?;
            v.sort();
            Ok(v.join("\n").into_bytes())
        }
        "write" => {
            let data = match b {
                Some(blob) => std::fs::read(blob)?,
                None => Vec::new(),
            };
            std::fs::write(&a, data).map(|()| Vec::new())
        }
        "remove" if a.is_dir() => std::fs::remove_dir_all(&a).map(|()| Vec::new()),
        "remove" => std::fs::remove_file(&a).map(|()| Vec::new()),
        "rename" => std::fs::rename(&a, b.unwrap_or_default()).map(|()| Vec::new()),
        "mkdir" => std::fs::create_dir_all(&a).map(|()| Vec::new()),
        "exists" => Ok(if a.exists() {
            b"1".to_vec()
        } else {
            b"0".to_vec()
        }),
        _ => Err(io::Error::other(format!("unknown op '{op}'"))),
    }
}

/// The watcher reports changes after they happen; this waits for them to settle.
pub(crate) fn settle() {
    std::thread::sleep(Duration::from_millis(400));
}

pub(crate) const T: i64 = crate::win::testkit::EPOCH_TIME + 100;

pub(crate) const V1: Files<'static> = &[
    ("a.txt", b"alpha"),
    ("b.txt", b"bravo"),
    ("c.txt", b"charlie"),
    ("d.txt", b"delta"),
    ("sub/x.txt", b"xray"),
    ("src/main.rs", b"fn main() {}"),
];

pub(crate) const V2: Files<'static> = &[
    ("a.txt", b"alpha two"),
    ("c.txt", b"charlie"),
    ("d.txt", b"delta"),
    ("src/main.rs", b"fn main() {}"),
    ("src/lib.rs", b"pub fn f() {}"),
    ("new/deep/n.txt", b"november"),
];

mod tests {
    use super::*;
    use crate::win::testkit::mtime;

    #[test]
    #[ignore = "run by Lab::other in a child process"]
    fn child() {
        let (Ok(spec), Ok(out)) = (std::env::var(OP_VAR), std::env::var(OUT_VAR)) else {
            return;
        };
        let out = PathBuf::from(out);
        match child_op(&spec) {
            Ok(bytes) => std::fs::write(&out, bytes).unwrap(),
            Err(e) => std::fs::write(err_path(&out), e.to_string()).unwrap(),
        }
    }

    #[test]
    fn directories_list_and_files_hydrate_from_the_tree() {
        let Some(lab) = Lab::new(V1) else { return };
        assert_eq!(
            lab.list(""),
            ["a.txt", "b.txt", "c.txt", "d.txt", "src", "sub"]
        );
        assert_eq!(lab.state("a.txt"), PathState::Placeholder);
        assert_eq!(std::fs::metadata(lab.abs("a.txt")).unwrap().len(), 5);
        assert_eq!(mtime(&lab.abs("a.txt")), crate::win::testkit::EPOCH_TIME);
        lab.hydrate(V1);
        assert_eq!(lab.state("a.txt"), PathState::Hydrated);
        let snap = lab.core.stats().snapshot();
        assert_eq!(snap.total_hydrations, V1.len() as u64, "{snap:?}");
        assert!(lab.core.stats().last_read_of(&p("a.txt")).is_some());
        // Reading changed nothing: placeholders are not edits.
        settle();
        let ops = lab.journal.dump();
        assert!(
            ops.iter()
                .all(|(op, _)| *op != Op::Modified && *op != Op::Deleted),
            "{ops:?}"
        );
    }

    #[test]
    fn the_deny_list_refuses_hydration_but_not_listing() {
        let Some(lab) = Lab::deny(V1, "cmd.exe") else {
            return;
        };
        let run = |script: &str| {
            Command::new("cmd")
                .args(["/d", "/c", script])
                .current_dir(lab.root())
                .output()
                .unwrap()
        };
        let listed = run("dir /a /b");
        assert!(String::from_utf8_lossy(&listed.stdout).contains("a.txt"));
        // `type` exits 0 even when the read fails; the content tells.
        assert!(run("type a.txt").stdout.is_empty());
        let snap = lab.core.stats().snapshot();
        assert!(
            snap.processes
                .iter()
                .any(|(n, s)| n == "cmd.exe" && s.denied > 0),
            "{snap:?}"
        );
        assert_eq!(lab.state("a.txt"), PathState::Placeholder);
        // Other images are not denied.
        assert_eq!(lab.read("a.txt").unwrap(), b"alpha");
    }

    #[test]
    fn edits_creations_deletions_and_renames_reach_the_journal() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.hydrate(V1);
        settle();
        lab.journal.clear();
        lab.write("a.txt", b"changed");
        lab.write("new.txt", b"new");
        lab.remove("b.txt").unwrap();
        lab.rename("c.txt", "c2.txt").unwrap();
        lab.rename("sub", "sub2").unwrap();
        assert!(lab.journal.wait_for(Op::Modified, "a.txt"));
        assert!(lab.journal.wait_for(Op::Created, "new.txt"));
        assert!(lab.journal.wait_for(Op::Deleted, "b.txt"));
        assert!(lab.journal.wait_for(Op::RenamedFrom, "c.txt"));
        assert!(lab.journal.wait_for(Op::Created, "c2.txt"));
        assert!(lab.journal.wait_for(Op::DirRenamedFrom, "sub"));
        assert!(lab.journal.wait_for(Op::DirRenamedTo, "sub2"));
        assert_eq!(lab.state("a.txt"), PathState::Full);
        assert_eq!(lab.read("sub2/x.txt").unwrap(), b"xray");
        let gone = lab.provider.tombstones().unwrap();
        assert!(gone.contains(&p("b.txt")), "{gone:?}");
        assert!(gone.contains(&p("c.txt")), "{gone:?}");
        assert!(gone.contains(&p("sub")), "{gone:?}");
        assert!(!gone.contains(&p("a.txt")), "{gone:?}");
    }

    #[test]
    fn a_directory_moved_before_it_was_listed_keeps_its_own_tree() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.rename("sub", "moved").unwrap();
        assert_eq!(lab.list("moved"), ["x.txt"]);
        assert_eq!(lab.read("moved/x.txt").unwrap(), b"xray");
    }

    #[test]
    fn locked_paths_are_never_deleted_or_moved_by_other_processes() {
        let files: Files<'_> = &[("sdk/a.h", b"int a;"), ("src/m.c", b"int m;")];
        let Some(lab) = Lab::locked(files, "sdk") else {
            return;
        };
        lab.hydrate(files);
        assert!(lab.remove("sdk/a.h").is_err());
        assert!(lab.rename("sdk/a.h", "sdk/b.h").is_err());
        assert!(lab.rename("src/m.c", "sdk/m.c").is_err());
        assert!(lab.exists("sdk/a.h") && lab.exists("src/m.c") && !lab.exists("sdk/m.c"));
        // Locked files are read-only on disk.
        assert!(
            std::fs::metadata(lab.abs("sdk/a.h"))
                .unwrap()
                .permissions()
                .readonly()
        );
        // An unlocked path moves.
        lab.rename("src/m.c", "src/n.c").unwrap();
        assert!(lab.exists("src/n.c"));
    }

    #[test]
    fn redirected_names_never_reach_the_journal() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.journal.clear();
        lab.mkdir("node_modules/pkg");
        lab.write("node_modules/pkg/i.js", b"x");
        lab.write("kept.txt", b"k");
        assert!(lab.journal.wait_for(Op::Created, "kept.txt"));
        settle();
        assert!(
            !lab.journal.any_under("node_modules"),
            "{:?}",
            lab.journal.dump()
        );
        lab.journal.clear();
        let r = lab.provider.reconcile().unwrap();
        assert!(
            !lab.journal.any_under("node_modules"),
            "{:?}",
            lab.journal.dump()
        );
        assert!(
            lab.journal.has(Op::Created, "kept.txt"),
            "{:?}",
            lab.journal.dump()
        );
        assert_eq!(r.created, 1, "{r:?}");
    }

    #[test]
    fn paths_longer_than_max_path_list_hydrate_and_journal() {
        let seg = "d".repeat(60);
        let deep = format!("{seg}/{seg}/{seg}/{seg}/{seg}/file.txt");
        let files: Vec<(&str, &[u8])> = vec![(deep.as_str(), b"deep")];
        let Some(lab) = Lab::new(&files) else { return };
        assert!(lab.abs(&deep).as_os_str().len() > 260);
        assert_eq!(lab.read(&deep).unwrap(), b"deep");
        lab.write(&deep, b"deeper");
        assert!(lab.journal.wait_for(Op::Modified, &deep));
    }

    #[test]
    fn a_safe_switch_updates_adds_and_deletes() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.hydrate(&V1[..2]);
        lab.list("sub");
        let r = lab.run(V1, V2, Mode::Safe, &[], T).unwrap();
        assert!(r.pending.is_empty() && r.dirty_skipped.is_empty(), "{r:?}");
        assert!(!lab.exists("b.txt"));
        assert!(!lab.exists("sub"));
        lab.hydrate(V2);
        assert_eq!(mtime(&lab.abs("a.txt")), T);
        assert_eq!(lab.list("new/deep"), ["n.txt"]);
        assert_eq!(lab.list("src"), ["lib.rs", "main.rs"]);
        // A second run is a no-op.
        let again = lab.run(V1, V2, Mode::Safe, &[], T).unwrap();
        assert!(
            again.pending.is_empty() && again.dirty_skipped.is_empty(),
            "{again:?}"
        );
        lab.hydrate(V2);
    }

    #[test]
    fn a_new_directory_appears_unlisted() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.hydrate(V1);
        lab.run(V1, V2, Mode::Safe, &[], T).unwrap();
        assert_eq!(lab.state("new"), PathState::Placeholder);
        assert!(
            super::super::ffi::probe(&lab.abs("new"))
                .unwrap()
                .is_unpopulated()
        );
        assert_eq!(lab.list("new"), ["deep"]);
    }

    #[test]
    fn safe_mode_leaves_edits_and_deletions_force_restores_them() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.hydrate(V1);
        lab.write("a.txt", b"mine");
        lab.remove("d.txt").unwrap();
        let v3: Files<'_> = &[
            ("a.txt", b"alpha three"),
            ("b.txt", b"bravo"),
            ("c.txt", b"charlie"),
            ("d.txt", b"delta three"),
            ("sub/x.txt", b"xray"),
            ("src/main.rs", b"fn main() {}"),
        ];
        let r = lab.run(V1, v3, Mode::Safe, &[], T).unwrap();
        assert_eq!(r.dirty_skipped, vec![p("a.txt"), p("d.txt")], "{r:?}");
        assert_eq!(lab.read("a.txt").unwrap(), b"mine");
        assert!(!lab.exists("d.txt"));
        let r = lab
            .run(V1, v3, Mode::Force, &["a.txt", "d.txt"], T)
            .unwrap();
        assert!(r.dirty_skipped.is_empty() && r.pending.is_empty(), "{r:?}");
        assert_eq!(lab.read("a.txt").unwrap(), b"alpha three");
        assert_eq!(lab.read("d.txt").unwrap(), b"delta three");
    }

    #[test]
    fn a_file_held_open_is_pending_then_resumed() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.hydrate(V1);
        let holder = Holder::new(&lab.abs("a.txt"));
        let r = lab.run(V1, V2, Mode::Safe, &[], T).unwrap();
        assert_eq!(r.pending, vec![p("a.txt")], "{r:?}");
        holder.release();
        let done = lab.provider.resume(T).unwrap().unwrap();
        assert_eq!(done.pending, 0);
        assert_eq!(lab.read("a.txt").unwrap(), b"alpha two");
        assert!(lab.provider.resume(T).unwrap().is_none());
    }

    #[test]
    fn reconcile_journals_what_changed_while_stopped() {
        let Some(mut lab) = Lab::new(V1) else { return };
        lab.hydrate(V1);
        let base = lab.tree(V1);
        lab.provider.unmount().unwrap();
        std::fs::write(lab.abs("a.txt"), b"offline edit").unwrap();
        std::fs::write(lab.abs("made.txt"), b"m").unwrap();
        std::fs::remove_file(lab.abs("b.txt")).unwrap();
        lab.restart(base);
        lab.journal.clear();
        let r = lab.provider.reconcile().unwrap();
        let ops = lab.journal.dump();
        assert!(lab.journal.has(Op::Modified, "a.txt"), "{ops:?}");
        assert!(lab.journal.has(Op::Created, "made.txt"), "{ops:?}");
        assert!(lab.journal.has(Op::Deleted, "b.txt"), "{ops:?}");
        assert!(!lab.journal.has(Op::Modified, "c.txt"), "{ops:?}");
        assert_eq!((r.modified, r.created, r.deleted), (1, 1, 1), "{r:?}");
    }

    #[test]
    fn dehydration_drops_clean_data_only() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.hydrate(V1);
        lab.write("c.txt", b"edited");
        assert!(lab.provider.hydrated_bytes().unwrap() > 0);
        let out = lab.provider.dehydrate(&[]).unwrap();
        assert_eq!(out.dehydrated, V1.len() - 1, "{out:?}");
        assert_eq!(lab.state("a.txt"), PathState::Placeholder);
        assert_eq!(lab.state("c.txt"), PathState::Full);
        assert_eq!(lab.read("c.txt").unwrap(), b"edited");
        assert_eq!(lab.provider.hydrated_bytes().unwrap(), 0);
        // It reads again.
        lab.hydrate(&V1[..1]);
        // A file held open is skipped.
        let holder = Holder::new(&lab.abs("a.txt"));
        let out = lab.provider.dehydrate(&[p("a.txt")]).unwrap();
        assert_eq!(out.skipped, vec![p("a.txt")], "{out:?}");
        holder.release();
    }

    #[test]
    fn unvirtualize_keeps_edits_and_drops_placeholders() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.hydrate(&V1[..2]);
        lab.write("a.txt", b"kept edit");
        lab.write("mine.txt", b"mine");
        lab.provider.unmount().unwrap();
        let done = super::super::unvirtualize(&lab.root()).unwrap();
        assert!(done.deleted.is_empty());
        assert!(done.dropped >= 1, "{done:?}");
        assert_eq!(std::fs::read(lab.abs("a.txt")).unwrap(), b"kept edit");
        assert_eq!(std::fs::read(lab.abs("mine.txt")).unwrap(), b"mine");
        assert!(!lab.abs("b.txt").exists());
        assert!(
            !super::super::ffi::probe(&lab.abs("a.txt"))
                .unwrap()
                .is_placeholder()
        );
        assert!(!super::super::ffi::is_our_sync_root(&lab.root()));
    }

    fn total(files: Files<'_>) -> u64 {
        files.iter().map(|(_, d)| d.len() as u64).sum()
    }

    fn eager() -> super::super::walk::Policy {
        super::super::walk::Policy {
            idle: Duration::ZERO,
            budget: u64::MAX,
        }
    }

    #[test]
    fn the_idle_pass_returns_clean_files_to_placeholders() {
        let Some(lab) = Lab::new(V1) else { return };
        lab.list("");
        assert_eq!(lab.provider.hydrated_bytes().unwrap(), 0);
        lab.hydrate(V1);
        assert_eq!(lab.provider.hydrated_bytes().unwrap(), total(V1));
        let r = lab
            .provider
            .dehydrate_idle(&eager(), std::time::SystemTime::now())
            .unwrap();
        assert_eq!(r.files, V1.len() as u64, "{r:?}");
        assert_eq!(r.bytes, total(V1));
        assert!(r.skipped.is_empty(), "{r:?}");
        assert_eq!(lab.provider.hydrated_bytes().unwrap(), 0);
        // The data is fetched again on the next read.
        let before = lab.core.stats().snapshot().total_hydrations;
        assert_eq!(lab.read("a.txt").unwrap(), b"alpha");
        assert_eq!(lab.core.stats().snapshot().total_hydrations, before + 1);
    }

    #[test]
    fn the_idle_pass_skips_recent_reads_and_keeps_to_the_budget() {
        let files: Files<'_> = &[
            ("one.txt", b"1111111111"),
            ("two.txt", b"2222222222"),
            ("three.txt", b"3333333333"),
        ];
        let Some(lab) = Lab::new(files) else { return };
        for (path, data) in files {
            assert_eq!(&lab.read(path).unwrap(), data);
            std::thread::sleep(Duration::from_millis(20));
        }
        let idle = super::super::walk::Policy {
            idle: super::super::walk::IDLE,
            budget: u64::MAX,
        };
        let now = std::time::SystemTime::now();
        let r = lab.provider.dehydrate_idle(&idle, now).unwrap();
        assert_eq!(r.files, 0, "read just now: {r:?}");
        let budget = super::super::walk::Policy {
            idle: super::super::walk::IDLE,
            budget: 10,
        };
        let r = lab.provider.dehydrate_idle(&budget, now).unwrap();
        assert_eq!(r.files, 2, "{r:?}");
        assert_eq!(lab.provider.hydrated_bytes().unwrap(), 10);
        // The newest read is the one left hydrated.
        assert_eq!(lab.state("three.txt"), PathState::Hydrated);
        assert_eq!(lab.state("one.txt"), PathState::Placeholder);
        // Days later nothing has been read for longer than the idle time.
        let later = now + Duration::from_secs(4 * 24 * 3600);
        let r = lab.provider.dehydrate_idle(&idle, later).unwrap();
        assert_eq!(r.files, 1, "{r:?}");
        assert_eq!(lab.provider.hydrated_bytes().unwrap(), 0);
    }

    #[test]
    fn a_root_registered_before_is_connected_to_as_it_is() {
        let Some(mut lab) = Lab::new(V1) else { return };
        lab.hydrate(V1);
        lab.provider.unmount().unwrap();
        let base = lab.tree(V1);
        lab.restart(base);
        // The listing survives the restart: nothing reads as deleted, and nothing lists twice.
        assert!(
            !super::super::ffi::probe(&lab.abs(""))
                .unwrap()
                .is_unpopulated()
        );
        assert!(lab.provider.reconcile().unwrap().deleted == 0);
        assert_eq!(
            lab.list(""),
            ["a.txt", "b.txt", "c.txt", "d.txt", "src", "sub"]
        );
    }
}
