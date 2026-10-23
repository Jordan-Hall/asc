//! The stash stack: `refs/stash` and its reflog.
//!
//! git keeps the stack as the ref log of `refs/stash` and rewrites that log on `drop`. The asc ref
//! log is append-only, so the stack is rebuilt by replaying the events of the log, oldest first:
//! a push inserts at the top, a `stash-drop` event removes the entry its reason names, and a
//! deleted ref empties the stack. The replay starts at the newest event that found the ref
//! absent and reads at most [`MAX_EVENTS`] rows (older entries fall off the bottom).

use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::RefName;
use asc_store::refs::{Actor, Expect, RefEntry, RefLogEntry, RefTxn, RefUpdate, Refs};
use uuid::Uuid;

use crate::cli::common::{CliError, Ctx, Result, StagingAfterMove, WsCtx};
use crate::cli::restore::actor;

/// The ref every stash lives on.
pub const REF: &str = "refs/stash";
/// Events read from the ref log.
const MAX_EVENTS: u32 = 10_000;
/// `Actor::via` of an event that pushed an entry.
pub const VIA_PUSH: &str = "stash-push";
/// `Actor::via` of an event that dropped an entry; the reason is `drop <index>`.
pub const VIA_DROP: &str = "stash-drop";
/// `Actor::via` of the event that cleared the stack.
pub const VIA_CLEAR: &str = "stash-clear";

/// One stash entry: the stash commit and its reflog message.
#[derive(Clone, Debug)]
pub struct Entry {
    pub commit: ChangesetId,
    pub message: String,
}

pub fn ref_name() -> Result<RefName> {
    RefName::new(REF.as_bytes().to_vec()).map_err(|e| CliError::Fatal(e.to_string()))
}

fn drop_index(reason: &str) -> Option<usize> {
    reason.strip_prefix("drop ")?.parse().ok()
}

/// The stack, newest entry first.
pub async fn load(ws: &WsCtx) -> Result<Vec<Entry>> {
    let name = ref_name()?;
    let current = ws.db.refs().get(ws.repo.key, &name).await?;
    let Some(RefEntry { target: RefTarget::Changeset(top), .. }) = current else {
        return Ok(Vec::new());
    };
    let mut events = ws.db.refs().log(ws.repo.key, &name, MAX_EVENTS).await?;
    if let Some(at) = events.iter().position(|e| e.old.is_none()) {
        events.truncate(at + 1);
    }
    let mut stack: Vec<Entry> = Vec::new();
    for e in events.iter().rev() {
        match &e.new {
            None => stack.clear(),
            Some(RefTarget::Changeset(c)) if e.actor.via == VIA_DROP => {
                let _ = c;
                if let Some(n) = drop_index(&e.reason)
                    && n < stack.len()
                {
                    stack.remove(n);
                }
            }
            Some(RefTarget::Changeset(c)) => stack.insert(0, Entry { commit: *c, message: e.reason.clone() }),
            Some(RefTarget::Tag(_)) => {}
        }
    }
    // A ref moved by other means (plumbing) still has its value at the top.
    if stack.first().map(|e| e.commit) != Some(top) {
        stack.insert(0, Entry { commit: top, message: String::new() });
    }
    Ok(stack)
}

/// The ref log `log -g` and `reflog` show for `name`: the replayed stack for `refs/stash`, as
/// entries newest first. `None` for any other ref.
pub async fn reflog_view(ws: &WsCtx, name: &RefName, limit: u32) -> Result<Option<Vec<RefLogEntry>>> {
    if name.as_bytes() != REF.as_bytes() {
        return Ok(None);
    }
    let mut out = Vec::new();
    for (i, e) in load(ws).await?.into_iter().take(limit as usize).enumerate() {
        let who = ws.engine.get_changeset(e.commit).await?.committer.name().to_vec();
        out.push(RefLogEntry {
            seq: i as i64,
            name: name.clone(),
            old: None,
            new: Some(RefTarget::Changeset(e.commit)),
            actor: Actor { id: String::from_utf8_lossy(&who).into_owned(), via: "stash".into() },
            reason: e.message,
            op_id: Uuid::nil(),
            at: 0,
        });
    }
    Ok(Some(out))
}

/// What `<stash>` named.
pub struct Picked {
    /// Position in the stack; `None` for a commit named by an arbitrary revision.
    pub index: Option<usize>,
    pub commit: ChangesetId,
    /// The text to print in `Dropped <text> (<sha>)`.
    pub text: String,
}

/// `stash@{n}`, `refs/stash@{n}` or `n`.
fn selector(text: &str) -> Option<usize> {
    let inner = text
        .strip_prefix("refs/stash@{")
        .or_else(|| text.strip_prefix("stash@{"))
        .and_then(|r| r.strip_suffix('}'))
        .unwrap_or(text);
    if inner.is_empty() || !inner.bytes().all(|b| b.is_ascii_digit()) || inner.len() > 9 {
        return None;
    }
    inner.parse().ok()
}

/// The entry `arg` names (the top one when absent); git's refusals when there is none.
pub async fn pick(ws: &WsCtx, stack: &[Entry], arg: Option<&str>) -> Result<Picked> {
    let Some(arg) = arg else {
        let top = stack.first().ok_or_else(|| CliError::Raw("No stash entries found.".into(), 1))?;
        return Ok(Picked { index: Some(0), commit: top.commit, text: "refs/stash@{0}".into() });
    };
    let Some(n) = selector(arg) else {
        return Err(CliError::Raw(format!("error: {arg} is not a valid reference"), 1));
    };
    let _ = ws;
    match stack.get(n) {
        Some(e) => {
            let text = if arg.chars().all(|c| c.is_ascii_digit()) { format!("stash@{{{n}}}") } else { arg.to_owned() };
            Ok(Picked { index: Some(n), commit: e.commit, text })
        }
        None if stack.is_empty() => Err(CliError::Raw(format!("error: {arg} is not a valid reference"), 1)),
        None => Err(CliError::Fatal(format!("log for 'stash' only has {} entries", stack.len()))),
    }
}

/// Moves `refs/stash` to `new` (`None` deletes it) and logs the event.
pub async fn move_ref(
    ctx: &Ctx,
    ws: &WsCtx,
    new: Option<ChangesetId>,
    via: &str,
    reason: &str,
) -> Result<()> {
    let name = ref_name()?;
    let current = ws.db.refs().get(ws.repo.key, &name).await?;
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name,
            expect: current.map_or(Expect::Absent, |e| Expect::Value(e.target)),
            new: new.map(RefTarget::Changeset),
            expires_at: None,
        }],
        actor: Actor { id: actor().id, via: via.to_owned() },
        reason: String::new(),
        op_id: Uuid::now_v7(),
    };
    ctx.update_local_refs(txn, reason, StagingAfterMove::Keep).await
}

/// Pushes `commit` onto the stack with `message` as its reflog text.
pub async fn push(ctx: &Ctx, ws: &WsCtx, commit: ChangesetId, message: &str) -> Result<()> {
    move_ref(ctx, ws, Some(commit), VIA_PUSH, message).await
}

/// Removes the entry at `index`.
pub async fn drop_at(ctx: &Ctx, ws: &WsCtx, stack: &[Entry], index: usize) -> Result<()> {
    let new_top = if index == 0 { stack.get(1).map(|e| e.commit) } else { stack.first().map(|e| e.commit) };
    move_ref(ctx, ws, new_top, VIA_DROP, &format!("drop {index}")).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors_accept_the_documented_forms_only() {
        assert_eq!(selector("stash@{2}"), Some(2));
        assert_eq!(selector("refs/stash@{0}"), Some(0));
        assert_eq!(selector("3"), Some(3));
        assert_eq!(selector("stash@{x}"), None);
        assert_eq!(selector("HEAD"), None);
        assert_eq!(selector("stash@{}"), None);
        assert_eq!(selector("99999999999"), None);
    }

    #[test]
    fn drop_reasons_round_trip() {
        assert_eq!(drop_index("drop 4"), Some(4));
        assert_eq!(drop_index("WIP on master: x"), None);
        assert_eq!(drop_index("drop x"), None);
    }
}
