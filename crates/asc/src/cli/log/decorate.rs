//! The ref decorations of `log --decorate` and `%d`: which refs point at which commit.

use std::collections::HashMap;

use asc_core::id::ChangesetId;
use asc_git::projection::Head;

use super::refs;
use crate::cli::common::{Result, WsCtx};
use crate::cli::format::decor::{Decor, DecorRef, RefKind};

fn kind_of(name: &str) -> Option<RefKind> {
    if name.starts_with("refs/heads/") {
        Some(RefKind::Local)
    } else if name.starts_with("refs/remotes/") {
        Some(RefKind::Remote)
    } else if name.starts_with("refs/tags/") {
        Some(RefKind::Tag)
    } else if name == "refs/stash" {
        Some(RefKind::Stash)
    } else {
        None
    }
}

/// Every decorated commit with its refs, `HEAD` first and the rest by descending name (git's
/// order).
pub async fn load(ws: &WsCtx) -> Result<HashMap<ChangesetId, Decor>> {
    let mut tips = refs::tips(ws, "refs/").await?;
    tips.sort_by(|a, b| b.name.cmp(&a.name));
    let mut map: HashMap<ChangesetId, Decor> = HashMap::new();
    for tip in tips {
        let Some(kind) = kind_of(&tip.name) else {
            continue;
        };
        map.entry(tip.commit).or_default().refs.push(DecorRef {
            kind,
            name: tip.name,
        });
    }
    if let Some(head) = ws.head_commit().await? {
        let d = map.entry(head).or_default();
        d.refs.insert(
            0,
            DecorRef {
                kind: RefKind::Head,
                name: "HEAD".into(),
            },
        );
        if let Ok(Head::Branch(b)) = ws.projection.read_head() {
            d.head_branch = Some(format!("refs/heads/{b}"));
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_branches_tags_remotes_and_stash_decorate() {
        assert_eq!(kind_of("refs/heads/a"), Some(RefKind::Local));
        assert_eq!(kind_of("refs/remotes/o/a"), Some(RefKind::Remote));
        assert_eq!(kind_of("refs/tags/a"), Some(RefKind::Tag));
        assert_eq!(kind_of("refs/stash"), Some(RefKind::Stash));
        assert_eq!(kind_of("refs/foo/bar"), None);
    }
}
