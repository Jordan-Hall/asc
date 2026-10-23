//! The state of a paused stack operation: `<gitdir>/asc-stack-op.json`, next to git's own state
//! files (SPEC §20.3). A restack that stops on a conflict leaves the conflicted change in git's
//! `rebase-merge/` and, here, the rest of its plan.
//!
//! The file is untrusted input when `asc continue` or `asc abort` reads it: it is size-capped, a
//! symlink is refused, unknown fields are refused, and every id, ref name and count is checked
//! before any of it is used.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use asc_core::id::ChangesetId;
use asc_core::path::RefName;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The file name inside the git directory.
pub const FILE: &str = "asc-stack-op.json";
/// Largest state file read.
const MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Most entries in any list of the file (the stack limit of the engine).
const MAX_ITEMS: usize = asc_engine::stack::MAX_STACK;
/// The only version this build reads.
const VERSION: u32 = 1;

/// A ref that follows the rewrite of the change it points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefFollow {
    pub name: RefName,
    pub from: ChangesetId,
}

/// A paused restack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StackOp {
    pub op_id: Uuid,
    /// The branch HEAD was on when the operation began; `None` for a detached HEAD.
    pub head_branch: Option<String>,
    /// HEAD's commit when the operation began.
    pub head: ChangesetId,
    pub follow: Vec<RefFollow>,
    /// Rewrites made before the stop, oldest first: `(old, new)`.
    pub done: Vec<(ChangesetId, ChangesetId)>,
    /// The change being replayed in `rebase-merge/`.
    pub at: ChangesetId,
    /// The commit the replay of `at` goes onto.
    pub at_onto: ChangesetId,
    /// Changes still to rewrite after `at`, oldest first.
    pub rest: Vec<ChangesetId>,
    /// Every rewritten commit is signed.
    pub sign: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u32,
    op: String,
    op_id: String,
    head_branch: Option<String>,
    head: String,
    follow: Vec<WireFollow>,
    done: Vec<WirePair>,
    at: String,
    at_onto: String,
    rest: Vec<String>,
    sign: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFollow {
    name: String,
    from: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePair {
    old: String,
    new: String,
}

fn bad(what: &str) -> String {
    format!("{FILE} is malformed: {what}")
}

fn id(s: &str) -> Result<ChangesetId, String> {
    s.parse().map_err(|_| bad("bad id"))
}

fn branch_ok(b: &str) -> bool {
    RefName::new(format!("refs/heads/{b}").into_bytes()).is_ok()
}

impl StackOp {
    fn to_wire(&self) -> Wire {
        Wire {
            version: VERSION,
            op: "restack".into(),
            op_id: self.op_id.to_string(),
            head_branch: self.head_branch.clone(),
            head: self.head.to_string(),
            follow: self
                .follow
                .iter()
                .map(|f| WireFollow { name: String::from_utf8_lossy(f.name.as_bytes()).into_owned(), from: f.from.to_string() })
                .collect(),
            done: self.done.iter().map(|(o, n)| WirePair { old: o.to_string(), new: n.to_string() }).collect(),
            at: self.at.to_string(),
            at_onto: self.at_onto.to_string(),
            rest: self.rest.iter().map(ToString::to_string).collect(),
            sign: self.sign,
        }
    }

    fn from_wire(w: Wire) -> Result<Self, String> {
        if w.version != VERSION || w.op != "restack" {
            return Err(bad("unknown version or operation"));
        }
        if w.follow.len() > MAX_ITEMS * 4 || w.done.len() > MAX_ITEMS || w.rest.len() > MAX_ITEMS {
            return Err(bad("too many entries"));
        }
        if let Some(b) = &w.head_branch
            && !branch_ok(b)
        {
            return Err(bad("bad branch name"));
        }
        let mut follow = Vec::with_capacity(w.follow.len());
        for f in &w.follow {
            let name = RefName::new(f.name.clone().into_bytes()).map_err(|_| bad("bad ref name"))?;
            if !f.name.starts_with("refs/heads/") {
                return Err(bad("a followed ref is not a branch"));
            }
            follow.push(RefFollow { name, from: id(&f.from)? });
        }
        let mut done = Vec::with_capacity(w.done.len());
        for p in &w.done {
            done.push((id(&p.old)?, id(&p.new)?));
        }
        let mut rest = Vec::with_capacity(w.rest.len());
        for r in &w.rest {
            rest.push(id(r)?);
        }
        Ok(Self {
            op_id: w.op_id.parse().map_err(|_| bad("bad operation id"))?,
            head_branch: w.head_branch,
            head: id(&w.head)?,
            follow,
            done,
            at: id(&w.at)?,
            at_onto: id(&w.at_onto)?,
            rest,
            sign: w.sign,
        })
    }
}

fn path(gitdir: &Path) -> PathBuf {
    gitdir.join(FILE)
}

/// Whether a stack operation is paused.
pub fn exists(gitdir: &Path) -> bool {
    std::fs::symlink_metadata(path(gitdir)).is_ok()
}

/// Reads the paused operation; `Ok(None)` when there is none.
pub fn load(gitdir: &Path) -> Result<Option<StackOp>, String> {
    let p = path(gitdir);
    let meta = match std::fs::symlink_metadata(&p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {FILE}: {e}")),
    };
    if !meta.is_file() || meta.len() > MAX_BYTES {
        return Err(bad("not a regular file of a sane size"));
    }
    let mut buf = Vec::new();
    std::fs::File::open(&p)
        .and_then(|f| f.take(MAX_BYTES + 1).read_to_end(&mut buf))
        .map_err(|e| format!("cannot read {FILE}: {e}"))?;
    if buf.len() as u64 > MAX_BYTES {
        return Err(bad("too large"));
    }
    let wire: Wire = serde_json::from_slice(&buf).map_err(|e| bad(&e.to_string()))?;
    StackOp::from_wire(wire).map(Some)
}

/// Writes the state through a temporary name and a rename, so a crash leaves the old content
/// or the new one and never a partial file.
pub fn save(gitdir: &Path, op: &StackOp) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(&op.to_wire()).map_err(std::io::Error::other)?;
    let tmp = gitdir.join(format!("{FILE}.tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    if let Err(e) = std::fs::rename(&tmp, path(gitdir)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Removes the state; a missing file is fine.
pub fn clear(gitdir: &Path) {
    let _ = std::fs::remove_file(path(gitdir));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cs(n: u8) -> ChangesetId {
        ChangesetId::from_bytes([n; 32])
    }

    fn sample() -> StackOp {
        StackOp {
            op_id: Uuid::nil(),
            head_branch: Some("feature".into()),
            head: cs(1),
            follow: vec![RefFollow { name: RefName::new(b"refs/heads/feature".to_vec()).unwrap(), from: cs(1) }],
            done: vec![(cs(2), cs(3))],
            at: cs(4),
            at_onto: cs(3),
            rest: vec![cs(5)],
            sign: true,
        }
    }

    #[test]
    fn round_trips_and_clears() {
        let d = tempfile::tempdir().unwrap();
        assert!(load(d.path()).unwrap().is_none());
        save(d.path(), &sample()).unwrap();
        assert!(exists(d.path()));
        assert_eq!(load(d.path()).unwrap(), Some(sample()));
        clear(d.path());
        assert!(!exists(d.path()));
    }

    #[test]
    fn refuses_garbage_unknown_fields_bad_ids_and_oversize() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(FILE);
        std::fs::write(&p, b"not json").unwrap();
        assert!(load(d.path()).is_err());
        let good = serde_json::to_string(&sample().to_wire()).unwrap();
        std::fs::write(&p, good.replace("\"sign\":true", "\"sign\":true,\"extra\":1")).unwrap();
        assert!(load(d.path()).is_err());
        std::fs::write(&p, good.replacen(&cs(5).to_string(), "zz", 1)).unwrap();
        assert!(load(d.path()).is_err());
        std::fs::write(&p, good.replace("refs/heads/feature", "refs/tags/feature")).unwrap();
        assert!(load(d.path()).is_err());
        std::fs::write(&p, good.replace("\"version\":1", "\"version\":2")).unwrap();
        assert!(load(d.path()).is_err());
        std::fs::write(&p, vec![b' '; MAX_BYTES as usize + 1]).unwrap();
        assert!(load(d.path()).is_err());
    }

    #[test]
    fn a_directory_in_place_of_the_file_is_refused() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join(FILE)).unwrap();
        assert!(load(d.path()).is_err());
    }
}
