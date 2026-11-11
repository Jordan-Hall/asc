//! The switch engine both Windows backends run (SPEC §10.2 item 8): `switch`, `reset --hard`,
//! `restore`.
//!
//! * The intent (`from_root`, `to_root`, the work set `W`) is persisted before anything is
//!   touched, and the base tree moves to `to_root` before the first path is applied, so a path
//!   that becomes virtual again is served from the new tree. A crash at any point is repaired by
//!   [`resume`], which re-runs the intent; every step is idempotent.
//! * Deletions run first, so a directory can become a file; directories they emptied are removed
//!   before the rest of the work runs.
//! * A path held open is retried at 100, 300 and 900 ms, then stays in the intent as pending;
//!   [`resume`] retries pending paths (the daemon calls it every 5 s).
//!
//! Recovery applies the SAFE rules: it never destroys a file with local data, so a crashed FORCE
//! switch leaves such files alone and the command is simply run again.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use asc_core::id::TreeId;
use asc_core::object::Change;
use asc_core::path::RepoPath;
use asc_state::{StateDb, WorkspaceId};
use asc_wc::intent::{self, Replayed};
use asc_wc::switch::{Mode, Plan};
use tokio::runtime::Handle;

use super::block;
use crate::core::{CoreFs, Result, SwitchOutcome, VfsError};

/// Delays before the three retries of a path that is held open.
pub const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_millis(300),
    Duration::from_millis(900),
];
/// Most threads that apply paths at once.
pub const MAX_THREADS: usize = 8;
/// How many `errors` a report keeps.
const MAX_ERRORS: usize = 16;

/// Where the switch intent is kept (`state.db`).
#[derive(Clone)]
pub struct Store {
    pub db: StateDb,
    pub ws: WorkspaceId,
}

/// One switch, reset or restore.
pub struct Request<'a> {
    pub mode: Mode,
    /// Updates and deletes, with no conflicts (the caller refused them in step 3).
    pub plan: &'a Plan,
    /// FORCE and restore: the dirty paths in the command's pathspec. Those the target tree has are
    /// restored, whatever their state (modified, full, read-only, tombstoned); those it lacks are
    /// untracked and left alone.
    pub extra: &'a BTreeSet<RepoPath>,
    pub from_root: TreeId,
    pub to_root: TreeId,
    pub to_head: &'a str,
}

/// What a switch did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Paths left in the intent: held open, or failed for another reason (see `errors`).
    pub pending: Vec<RepoPath>,
    /// Paths with local data that SAFE or MERGE mode left alone, as step 3 requires.
    pub dirty_skipped: Vec<RepoPath>,
    /// Paths written (updated, deleted or made virtual).
    pub applied: usize,
    /// The first few reasons paths stayed pending.
    pub errors: Vec<String>,
}

impl Report {
    pub fn outcome(&self) -> SwitchOutcome {
        SwitchOutcome {
            pending: self.pending.clone(),
        }
    }
}

/// How a path of `W` is treated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Work {
    Update,
    /// The target tree deletes the path.
    Delete,
    /// A dirty path restored from the target tree, if it has it.
    Extra,
}

/// What applying one path did.
#[derive(Debug)]
pub enum Out {
    Done,
    DirtySkipped,
    InUse(String),
}

/// How one backend brings a path to the target version.
pub trait Apply: Sync {
    /// Brings `path` to the target tree (the base tree has already moved there).
    fn apply(&self, path: &RepoPath, work: Work) -> io::Result<Out>;
    /// After deletions: directories the target no longer has are removed bottom-up when empty.
    fn remove_empty_dirs(&self, deleted: &[RepoPath]);
    /// After all paths: makes what the switch added visible to watchers and listings.
    fn finish(&self) {}
}

pub(crate) fn vfs(e: impl ToString) -> io::Error {
    io::Error::other(e.to_string())
}

/// Runs `f` over `items` on up to [`MAX_THREADS`] threads.
pub(crate) fn par_each<T: Sync>(items: &[T], f: impl Fn(&T) + Sync) {
    if items.is_empty() {
        return;
    }
    let threads = std::thread::available_parallelism()
        .map_or(2, std::num::NonZero::get)
        .clamp(1, MAX_THREADS)
        .min(items.len());
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(item) = items.get(i) else { return };
                    f(item);
                }
            });
        }
    });
}

/// Outcomes of one pass over a batch.
#[derive(Default)]
struct Pass {
    done: Vec<RepoPath>,
    dirty: Vec<RepoPath>,
    busy: Vec<(RepoPath, Work, String)>,
}

fn pass(run: &dyn Apply, items: &[(RepoPath, Work)]) -> Pass {
    let out = Mutex::new(Pass::default());
    par_each(items, |(path, work)| {
        let r = run.apply(path, *work);
        let mut o = out.lock().unwrap_or_else(|e| e.into_inner());
        match r {
            Ok(Out::Done) => o.done.push(path.clone()),
            Ok(Out::DirtySkipped) => o.dirty.push(path.clone()),
            Ok(Out::InUse(why)) => o.busy.push((path.clone(), *work, why)),
            Err(e) => o.busy.push((path.clone(), *work, e.to_string())),
        }
    });
    out.into_inner().unwrap_or_else(|e| e.into_inner())
}

/// Applies `work`: deletions first, so a directory can become a file; held-open paths are retried
/// after 100, 300 and 900 ms.
pub fn execute(run: &dyn Apply, work: &BTreeMap<RepoPath, Work>) -> Report {
    let (deletes, rest): (Vec<_>, Vec<_>) = work
        .iter()
        .map(|(p, w)| (p.clone(), *w))
        .partition(|(_, w)| *w == Work::Delete);
    let mut report = Report::default();
    let mut deleted: Vec<RepoPath> = Vec::new();
    let mut busy: Vec<(RepoPath, Work, String)> = Vec::new();
    let is_delete = |d: &&RepoPath| work.get(*d) == Some(&Work::Delete);
    for batch in [deletes, rest] {
        let p = pass(run, &batch);
        report.applied += p.done.len();
        deleted.extend(p.done.iter().filter(is_delete).cloned());
        report.dirty_skipped.extend(p.dirty);
        busy.extend(p.busy);
        // Directories emptied by the first batch are removed before the second runs.
        run.remove_empty_dirs(&deleted);
    }
    for delay in RETRY_DELAYS {
        if busy.is_empty() {
            break;
        }
        std::thread::sleep(delay);
        let items: Vec<(RepoPath, Work)> = busy.iter().map(|(p, w, _)| (p.clone(), *w)).collect();
        let p = pass(run, &items);
        report.applied += p.done.len();
        deleted.extend(p.done.iter().filter(is_delete).cloned());
        report.dirty_skipped.extend(p.dirty);
        busy = p.busy;
    }
    run.remove_empty_dirs(&deleted);
    busy.sort_by(|a, b| a.0.cmp(&b.0));
    for (p, _, why) in busy {
        if report.errors.len() < MAX_ERRORS {
            report
                .errors
                .push(format!("{}: {why}", String::from_utf8_lossy(p.as_bytes())));
        }
        report.pending.push(p);
    }
    report.dirty_skipped.sort();
    report
}

/// The work set of a request.
pub fn work_of(req: &Request<'_>) -> Result<BTreeMap<RepoPath, Work>> {
    let mut work: BTreeMap<RepoPath, Work> = BTreeMap::new();
    for (p, _) in &req.plan.updates {
        work.insert(p.clone(), Work::Update);
    }
    for p in &req.plan.deletes {
        work.insert(p.clone(), Work::Delete);
    }
    for p in req.extra {
        work.entry(p.clone()).or_insert(Work::Extra);
    }
    if work.len() > intent::MAX_PENDING {
        return Err(VfsError::Io(vfs(format!(
            "the switch touches {} paths, more than the limit of {}",
            work.len(),
            intent::MAX_PENDING
        ))));
    }
    Ok(work)
}

/// Runs the switch of `req` with the applier `make` builds (called after the base tree moved).
pub fn switch<A: Apply>(
    store: Option<&Store>,
    core: &CoreFs,
    rt: &Handle,
    req: &Request<'_>,
    make: impl FnOnce() -> A,
) -> Result<Report> {
    if !req.plan.conflicts.is_empty() {
        return Err(VfsError::Conflicts);
    }
    let work = work_of(req)?;
    let all: BTreeSet<RepoPath> = work.keys().cloned().collect();
    if let Some(st) = store {
        block(
            rt,
            intent::begin(&st.db, st.ws, req.from_root, req.to_root, req.to_head, &all),
        )
        .map_err(vfs)?;
    }
    // The new base tree serves every path that becomes virtual from here on.
    core.set_root(req.to_root);
    let run = make();
    let report = execute(&run, &work);
    run.finish();
    if let Some(st) = store {
        let rest: BTreeSet<RepoPath> = report.pending.iter().cloned().collect();
        if rest.is_empty() {
            block(rt, intent::clear(&st.db, st.ws)).map_err(vfs)?;
        } else {
            block(
                rt,
                intent::begin(
                    &st.db,
                    st.ws,
                    req.from_root,
                    req.to_root,
                    req.to_head,
                    &rest,
                ),
            )
            .map_err(vfs)?;
        }
    }
    Ok(report)
}

/// Re-runs the stored intent with SAFE rules: after a restart (the interrupted switch completes)
/// and every 5 s for paths that were held open. `make` is given the intent's `from_root`. `None`
/// when there is no intent.
pub fn resume<A: Apply>(
    store: Option<&Store>,
    core: &CoreFs,
    rt: &Handle,
    make: impl FnOnce(TreeId) -> A,
) -> Result<Option<Replayed>> {
    let Some(st) = store else {
        return Ok(None);
    };
    let Some(current) = block(rt, intent::load(&st.db, st.ws)).map_err(vfs)? else {
        return Ok(None);
    };
    core.set_root(current.to_root);
    let run = make(current.from_root);
    // The same as `intent::replay` (applied paths leave the list, busy ones stay, an error saves
    // the progress), but synchronous: the apply step blocks on the runtime.
    let (mut keep, mut applied, mut failure) = (Vec::new(), 0usize, None);
    for (i, raw) in current.pending.iter().enumerate() {
        let done = RepoPath::new(raw.as_slice())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
            .and_then(|p| run.apply(&p, Work::Extra));
        match done {
            Ok(Out::Done | Out::DirtySkipped) => applied += 1,
            Ok(Out::InUse(_)) => keep.push(raw.clone()),
            Err(e) => {
                keep.extend(current.pending[i..].iter().cloned());
                failure = Some(e);
                break;
            }
        }
    }
    let rest: BTreeSet<RepoPath> = keep
        .iter()
        .filter_map(|r| RepoPath::new(r.as_slice()).ok())
        .collect();
    if rest.is_empty() {
        block(rt, intent::clear(&st.db, st.ws)).map_err(vfs)?;
    } else if applied > 0 {
        block(
            rt,
            intent::begin(
                &st.db,
                st.ws,
                current.from_root,
                current.to_root,
                &current.to_head,
                &rest,
            ),
        )
        .map_err(vfs)?;
    }
    run.finish();
    match failure {
        Some(e) => Err(e.into()),
        None => Ok(Some(Replayed {
            applied,
            pending: rest.len(),
        })),
    }
}

/// The `Provider::apply_switch` entry: SAFE rules, no extras, the base tree unchanged.
pub fn apply_plan(run: &dyn Apply, plan: &Plan) -> Result<SwitchOutcome> {
    if !plan.conflicts.is_empty() {
        return Err(VfsError::Conflicts);
    }
    let mut work: BTreeMap<RepoPath, Work> = BTreeMap::new();
    for (p, c) in &plan.updates {
        debug_assert!(!matches!(c, Change::Delete));
        work.insert(p.clone(), Work::Update);
    }
    for p in &plan.deletes {
        work.insert(p.clone(), Work::Delete);
    }
    let report = execute(run, &work);
    run.finish();
    Ok(report.outcome())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;

    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    /// Records the order of calls; `busy` paths stay in use for the first `busy_for` attempts.
    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<(String, Work)>>,
        removed: Mutex<Vec<Vec<String>>>,
        busy: Vec<&'static str>,
        busy_for: u32,
        attempts: AtomicU32,
        dirty: Vec<&'static str>,
    }

    impl Apply for Fake {
        fn apply(&self, path: &RepoPath, work: Work) -> io::Result<Out> {
            let name = String::from_utf8_lossy(path.as_bytes()).into_owned();
            self.calls.lock().unwrap().push((name.clone(), work));
            if self.dirty.contains(&name.as_str()) {
                return Ok(Out::DirtySkipped);
            }
            if self.busy.contains(&name.as_str())
                && self.attempts.fetch_add(1, Ordering::SeqCst) < self.busy_for
            {
                return Ok(Out::InUse("held open".into()));
            }
            Ok(Out::Done)
        }

        fn remove_empty_dirs(&self, deleted: &[RepoPath]) {
            self.removed.lock().unwrap().push(
                deleted
                    .iter()
                    .map(|d| String::from_utf8_lossy(d.as_bytes()).into_owned())
                    .collect(),
            );
        }
    }

    fn work(items: &[(&str, Work)]) -> BTreeMap<RepoPath, Work> {
        items.iter().map(|(s, w)| (p(s), *w)).collect()
    }

    #[test]
    fn deletions_run_before_updates_and_empty_dirs_go_between() {
        let fake = Fake::default();
        let r = execute(
            &fake,
            &work(&[
                ("a/x", Work::Delete),
                ("a", Work::Update),
                ("b", Work::Extra),
            ]),
        );
        assert_eq!(r.applied, 3);
        let calls = fake.calls.lock().unwrap();
        assert_eq!(calls[0], ("a/x".to_owned(), Work::Delete));
        // After the delete batch, the deleted path was offered for directory cleanup.
        assert_eq!(fake.removed.lock().unwrap()[0], vec!["a/x".to_owned()]);
        assert!(r.pending.is_empty() && r.errors.is_empty());
    }

    #[test]
    fn a_held_open_path_is_retried_then_left_pending() {
        let fake = Fake {
            busy: vec!["held"],
            busy_for: u32::MAX,
            ..Fake::default()
        };
        let r = execute(
            &fake,
            &work(&[("held", Work::Update), ("ok", Work::Update)]),
        );
        assert_eq!(r.applied, 1);
        assert_eq!(r.pending, vec![p("held")]);
        assert_eq!(r.errors, vec!["held: held open".to_owned()]);
        // One first attempt and one per retry delay.
        assert_eq!(
            fake.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(n, _)| n == "held")
                .count(),
            1 + RETRY_DELAYS.len()
        );
    }

    #[test]
    fn a_path_free_on_retry_is_applied() {
        let fake = Fake {
            busy: vec!["held"],
            busy_for: 1,
            ..Fake::default()
        };
        let r = execute(&fake, &work(&[("held", Work::Update)]));
        assert_eq!(r.applied, 1);
        assert!(r.pending.is_empty());
    }

    #[test]
    fn dirty_paths_are_reported_sorted() {
        let fake = Fake {
            dirty: vec!["z", "m"],
            ..Fake::default()
        };
        let r = execute(&fake, &work(&[("z", Work::Update), ("m", Work::Update)]));
        assert_eq!(r.dirty_skipped, vec![p("m"), p("z")]);
        assert_eq!(r.applied, 0);
    }

    #[test]
    fn a_plan_with_conflicts_is_refused() {
        let plan = Plan {
            conflicts: vec![p("c")],
            ..Plan::default()
        };
        assert!(matches!(
            apply_plan(&Fake::default(), &plan),
            Err(VfsError::Conflicts)
        ));
    }

    #[test]
    fn the_work_set_merges_extras_without_overriding() {
        let plan = Plan {
            updates: vec![(
                p("u"),
                Change::Gitlink {
                    commit: asc_core::id::GitOid::from_bytes([0; 20]),
                },
            )],
            deletes: vec![p("d")],
            ..Plan::default()
        };
        let extra: BTreeSet<RepoPath> = [p("u"), p("e")].into_iter().collect();
        let req = Request {
            mode: Mode::Force,
            plan: &plan,
            extra: &extra,
            from_root: TreeId::from_bytes([0; 32]),
            to_root: TreeId::from_bytes([1; 32]),
            to_head: "refs/heads/main",
        };
        let w = work_of(&req).unwrap();
        assert_eq!(w[&p("u")], Work::Update);
        assert_eq!(w[&p("d")], Work::Delete);
        assert_eq!(w[&p("e")], Work::Extra);
    }
}
