//! The writable mounts of a push (SPEC §10.6). Each mount whose head is not yet on its branch
//! goes with the parent: on the same server the child's commits are uploaded first without a ref
//! and its branch moves in the parent's `UpdateRefs` as a linked group, so both land or neither
//! does; on another server the child is pushed on its own, first, and the line says
//! `linked: false`.

use std::sync::Arc;

use asc_core::id::RefTarget;
use asc_engine::sync::{PushMode, PushUpdate};
use asc_store::refs::{Expect, RefUpdate};

use super::common::{Ctx, Result, WsCtx};
use super::fetch::sync_error;
use super::remote::apply_refs;
use super::submodule::mount::{Mount, mounts_here};
use super::switch::ref_name;
use super::worktree::open_at;
use crate::remote::server::{LinkedRefs, ServerRemote};
use crate::remote::{self, Intent, StateBoundaries};

/// The child's remote a push uses: `remote.pushDefault`, else `origin` (a mount is detached).
fn child_remote(child: &Ctx) -> String {
    child
        .config
        .get("remote.pushDefault")
        .filter(|r| !r.trim().is_empty())
        .unwrap_or_else(|| "origin".to_owned())
}

/// A mount's branch that moves with the parent.
pub struct Pending {
    pub path: String,
    pub branch: String,
    pub linked: bool,
    child: Ctx,
    remote: String,
    configured: bool,
    new: RefTarget,
}

/// Pushes every mount that has commits its remote branch lacks. The ones on the parent's
/// server are uploaded only; their ref updates are returned for the parent's linked group.
/// `dry_run` reports the plan and sends nothing.
pub async fn prepare(
    ctx: &Ctx,
    ws: &WsCtx,
    server: Option<&Arc<ServerRemote>>,
    dry_run: bool,
) -> Result<(Vec<LinkedRefs>, Vec<Pending>)> {
    let mut linked = Vec::new();
    let mut pending = Vec::new();
    for m in mounts_here(ctx, ws).await? {
        if let Some((group, p)) = one(ctx, server, &m, dry_run).await? {
            linked.extend(group);
            pending.push(p);
        }
    }
    Ok((linked, pending))
}

async fn one(
    ctx: &Ctx,
    server: Option<&Arc<ServerRemote>>,
    m: &Mount,
    dry_run: bool,
) -> Result<Option<(Option<LinkedRefs>, Pending)>> {
    let child = open_at(ctx, m.root()).await?;
    let cws = child.ws()?;
    let Some(head) = cws.head_commit().await? else {
        return Ok(None);
    };
    let branch = m.branch(ctx);
    let dst = ref_name(&format!("refs/heads/{branch}"))?;
    let name = child_remote(&child);
    let opened = remote::open(&child, cws, &name, Intent::Write).await?;
    let current = opened
        .remote
        .list_refs()
        .await
        .map_err(sync_error)?
        .into_iter()
        .find(|r| r.name == dst)
        .map(|r| r.target);
    let new = RefTarget::Changeset(head);
    if current == Some(new) {
        return Ok(None);
    }
    let expect = current.map_or(Expect::Absent, Expect::Value);
    let same = match (server, &opened.server) {
        (Some(a), Some(b)) => a.target().rpc_url == b.target().rpc_url,
        _ => false,
    };
    let p = Pending {
        path: m.path.to_string(),
        branch: branch.clone(),
        linked: same,
        remote: name,
        configured: opened.configured,
        new,
        child: open_at(ctx, m.root()).await?,
    };
    if dry_run {
        return Ok(Some((None, p)));
    }
    remote::claim_local_refs(cws, &opened).await?;
    let boundaries = StateBoundaries::new(cws);
    let opts = remote::options(&child, &boundaries);
    let upd = [PushUpdate {
        dst: dst.clone(),
        new: Some(new),
        expect,
    }];
    if same {
        cws.engine
            .push(&*opened.remote, &upd, PushMode::UploadOnly, &opts)
            .await
            .map_err(sync_error)?;
        let repo = opened
            .server
            .as_ref()
            .map(|s| s.target().repo.clone())
            .unwrap_or_default();
        let group = LinkedRefs {
            repo,
            updates: vec![RefUpdate {
                name: dst,
                expect,
                new: Some(new),
                expires_at: None,
            }],
        };
        Ok(Some((Some(group), p)))
    } else {
        cws.engine
            .push(&*opened.remote, &upd, PushMode::ForceWithLease, &opts)
            .await
            .map_err(sync_error)?;
        println!(
            "{}: pushed {branch} to {} (linked: false)",
            m.path, p.remote
        );
        p.track().await?;
        Ok(Some((None, p)))
    }
}

impl Pending {
    /// Moves the child's remote-tracking ref to what the push stored.
    pub async fn track(&self) -> Result<()> {
        if !self.configured {
            return Ok(());
        }
        let name = ref_name(&format!("refs/remotes/{}/{}", self.remote, self.branch))?;
        apply_refs(
            &self.child,
            vec![RefUpdate {
                name,
                expect: Expect::Any,
                new: Some(self.new),
                expires_at: None,
            }],
            "update by push",
        )
        .await
    }
}
