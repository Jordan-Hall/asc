//! Directory listings: what a directory placeholder lists (from the identity it carries, never
//! from what now stands at its path), the placeholders built from it, and the in-process
//! population the provider itself runs before it walks or lists a directory the filter has not
//! listed yet.

use std::collections::HashSet;
use std::io;
use std::path::Path;

use asc_core::id::TreeId;
use asc_core::object::Kind;
use asc_core::path::RepoPath;

use super::Ident;
use super::ffi::{self, NewPlaceholder};
use super::provider::{Inner, vfs_io};
use crate::core::{CoreFs, DirListing, Node};
use crate::win::block;

/// What the directory at `path` lists when its placeholder says `ident`. A directory moved before
/// it was listed lists its own tree; a gitlink without a composed tree lists nothing.
pub(crate) async fn listing_for(
    core: &CoreFs,
    path: Option<&RepoPath>,
    ident: Option<Ident>,
) -> crate::core::Result<DirListing> {
    let Some(path) = path else {
        return Ok(core.read_dir(None).await?.unwrap_or_default());
    };
    let id = match ident {
        Some(Ident::Tree(id)) => id,
        // The root identity is never on a child; anything else is not a directory of ours.
        Some(Ident::Root | Ident::File { .. }) | None => return Ok(DirListing::default()),
    };
    if id == [0; 32] {
        return Ok(DirListing::default());
    }
    if let Some(n) = core.resolve(path).await?
        && n.id == id
        && matches!(n.kind, Kind::Dir | Kind::Gitlink)
    {
        return Ok(core.read_dir(Some(path)).await?.unwrap_or_default());
    }
    core.read_tree_at(TreeId::from_bytes(id), Some(path), false)
        .await
}

/// The placeholder of `node` with time `t`. A symlink is a file holding its target.
pub(crate) fn placeholder_of(node: &Node, t: i64) -> Option<NewPlaceholder> {
    let name = String::from_utf8(node.path.file_name().as_bytes().to_vec()).ok()?;
    let dir = matches!(node.kind, Kind::Dir | Kind::Gitlink);
    Some(NewPlaceholder {
        name,
        identity: Ident::of(node).encode(),
        meta: ffi::metadata(dir, node.size, t, node.read_only && !dir),
        dir,
    })
}

/// The placeholders of every entry of `listing`.
pub(crate) fn placeholders(inner: &Inner, listing: &DirListing) -> io::Result<Vec<NewPlaceholder>> {
    let paths: Vec<RepoPath> = listing.entries.iter().map(|n| n.path.clone()).collect();
    let times = inner.times(&paths)?;
    Ok(listing
        .entries
        .iter()
        .zip(times)
        .filter_map(|(n, t)| placeholder_of(n, t))
        .collect())
}

/// Lists the directory at `path` (on disk at `abs`) the way the filter would: the placeholders
/// of every child not already there are created, and the directory stops asking for its
/// listing. A child that already stands there (made by the user before the listing) is kept.
/// Returns how many placeholders were made.
pub(crate) fn populate(inner: &Inner, path: Option<&RepoPath>, abs: &Path) -> io::Result<usize> {
    let ident = ffi::probe(abs)?.ident();
    let ident = if path.is_none() {
        Some(Ident::Root)
    } else {
        ident
    };
    let listing =
        block(&inner.cfg.rt, listing_for(&inner.cfg.core, path, ident)).map_err(vfs_io)?;
    let present = on_disk_names(abs)?;
    let wanted: Vec<_> = placeholders(inner, &listing)?
        .into_iter()
        .filter(|p| !present.contains(&p.name.to_lowercase()))
        .collect();
    let made = if wanted.is_empty() {
        0
    } else {
        ffi::create_placeholders(abs, &wanted)?
    };
    ffi::disable_on_demand(abs)?;
    Ok(made)
}

/// The names on disk in `abs`, lower case. Reading a directory from this process lists only what
/// is on disk: the filter does not ask its own provider for a listing.
pub(crate) fn on_disk_names(abs: &Path) -> io::Result<HashSet<String>> {
    let mut out = HashSet::new();
    for e in std::fs::read_dir(abs)? {
        let e = e?;
        if let Some(n) = e.file_name().to_str() {
            out.insert(n.to_lowercase());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(path: &str, kind: Kind, size: u64, ro: bool) -> Node {
        Node {
            path: RepoPath::new(path).unwrap(),
            kind,
            id: [5; 32],
            size,
            git: asc_core::id::GitOid::from_bytes([0; 20]),
            read_only: ro,
        }
    }

    #[test]
    fn placeholders_carry_identity_size_and_lock() {
        let f = placeholder_of(&node("src/lib.rs", Kind::File, 12, false), 100).unwrap();
        assert_eq!(f.name, "lib.rs");
        assert!(!f.dir);
        assert_eq!(f.meta.FileSize, 12);
        assert_eq!(
            Ident::decode(&f.identity),
            Some(Ident::of(&node("src/lib.rs", Kind::File, 12, false)))
        );
        let ro = placeholder_of(&node("sdk/a.h", Kind::File, 1, true), 100).unwrap();
        assert_eq!(
            ro.meta.BasicInfo.FileAttributes,
            ffi::FILE_ATTRIBUTE_READONLY
        );
        let d = placeholder_of(&node("src", Kind::Dir, 0, true), 100).unwrap();
        assert!(d.dir);
        assert_eq!(
            d.meta.BasicInfo.FileAttributes,
            ffi::FILE_ATTRIBUTE_DIRECTORY
        );
        let g = placeholder_of(&node("vendor/lib", Kind::Gitlink, 0, false), 100).unwrap();
        assert!(g.dir);
        assert!(Ident::decode(&g.identity).is_some_and(|i| i.is_tree()));
        let s = placeholder_of(&node("link", Kind::Symlink, 6, false), 100).unwrap();
        assert!(!s.dir);
    }
}
