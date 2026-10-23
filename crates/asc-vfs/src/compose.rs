//! Read-only composition of submodules (SPEC §10.6, §15.5): the [`Compose`] hook of
//! [`CoreFs`](crate::core::CoreFs) filled from the nested repository's `git_map`.
//!
//! A [`Composition`] is built before a mount (or a switch) from the pins of the parent tree: each
//! gitlink's nested repository comes from [`Submodules`], and the [`Nested`] resolver maps the
//! pinned git commit to the nested changeset's root tree, fetching from the nested remote when
//! needed. The resolver is where read authorisation happens: a denied nested repository is
//! projected as an empty read-only directory (the restricted projection), never an error and
//! never a leak of its contents or of whether the commit exists.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use asc_core::id::{GitOid, TreeId};
use asc_core::object::Tree;
use asc_core::path::RepoPath;
use asc_wc::compose_plain::{Submodules, restricted_line};

use crate::core::{Compose, CoreFs, VfsError};

/// Most gitlinks composed for one tree.
pub const MAX_PINS: usize = 4096;

/// Why the nested tree is not available.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NestedError {
    /// The caller may not read the nested repository.
    Denied,
    /// The nested repository has no changeset for the commit, even after a fetch.
    Unmapped,
    /// Anything else (network, store); the composition fails.
    Other(String),
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Resolves the root tree of a nested repository at a git commit.
pub trait Nested: Send + Sync {
    /// The root tree of `repo`'s changeset for `commit`: authorise read on `repo` first, look the
    /// commit up in the nested git map, and fetch from the nested remote when it is not there.
    fn root_tree<'a>(
        &'a self,
        repo: &'a str,
        commit: &'a GitOid,
    ) -> BoxFuture<'a, Result<TreeId, NestedError>>;
}

/// What one gitlink projects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Projection {
    /// The nested tree, read-only.
    Tree { repo: String, tree: TreeId },
    /// The caller has no access to `repo`: an empty read-only directory.
    Restricted { repo: String },
    /// No nested repository is configured, or its commit is unknown: the gitlink stays an empty
    /// directory, as in a git checkout with an uninitialised submodule.
    Unresolved { repo: Option<String> },
}

/// Failures of building a composition.
#[derive(Debug, thiserror::Error)]
pub enum ComposeError {
    #[error("more than {MAX_PINS} gitlinks")]
    TooMany,
    #[error("nested repository {repo}: {message}")]
    Nested { repo: String, message: String },
}

/// The id of the empty tree.
pub fn empty_tree() -> TreeId {
    Tree::new(Vec::new(), true)
        .expect("the empty tree is valid")
        .id()
}

/// The projection of every gitlink of one parent tree.
#[derive(Debug, Default)]
pub struct Composition {
    pins: HashMap<(RepoPath, GitOid), Projection>,
}

impl Composition {
    /// Resolves each `(path, commit)` pin. Duplicate pins are resolved once.
    pub async fn build(
        pins: &[(RepoPath, GitOid)],
        subs: &Submodules,
        nested: &dyn Nested,
    ) -> Result<Self, ComposeError> {
        if pins.len() > MAX_PINS {
            return Err(ComposeError::TooMany);
        }
        let mut out = HashMap::with_capacity(pins.len());
        for (path, commit) in pins {
            let key = (path.clone(), *commit);
            if out.contains_key(&key) {
                continue;
            }
            let projection = match subs.repo_for(path) {
                None => Projection::Unresolved { repo: None },
                Some(repo) => match nested.root_tree(repo, commit).await {
                    Ok(tree) => Projection::Tree {
                        repo: repo.to_owned(),
                        tree,
                    },
                    Err(NestedError::Denied) => Projection::Restricted {
                        repo: repo.to_owned(),
                    },
                    Err(NestedError::Unmapped) => Projection::Unresolved {
                        repo: Some(repo.to_owned()),
                    },
                    Err(NestedError::Other(message)) => {
                        return Err(ComposeError::Nested {
                            repo: repo.to_owned(),
                            message,
                        });
                    }
                },
            };
            out.insert(key, projection);
        }
        Ok(Self { pins: out })
    }

    pub fn projection(&self, path: &RepoPath, commit: &GitOid) -> Option<&Projection> {
        self.pins.get(&(path.clone(), *commit))
    }

    /// The `<path>: restricted (no access to <repo>)` lines, sorted by path.
    pub fn restricted_lines(&self) -> Vec<String> {
        let mut rows: Vec<(&RepoPath, String)> = self
            .pins
            .iter()
            .filter_map(|((path, _), p)| match p {
                Projection::Restricted { repo } => Some((path, restricted_line(path, repo))),
                _ => None,
            })
            .collect();
        rows.sort_by(|a, b| a.0.cmp(b.0));
        rows.into_iter().map(|(_, line)| line).collect()
    }
}

impl Compose for Composition {
    fn subtree(&self, path: &RepoPath, commit: &GitOid) -> Option<TreeId> {
        match self.projection(path, commit)? {
            Projection::Tree { tree, .. } => Some(*tree),
            Projection::Restricted { .. } => Some(empty_tree()),
            Projection::Unresolved { .. } => None,
        }
    }
}

/// A write below a composed gitlink was refused (`EACCES` / `ERROR_ACCESS_DENIED`).
#[derive(Debug, thiserror::Error)]
#[error("access denied: {0} is part of a read-only composed submodule")]
pub struct WriteDenied(pub RepoPath);

/// Failures of [`check_write`].
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error(transparent)]
    Denied(#[from] WriteDenied),
    #[error(transparent)]
    Vfs(#[from] VfsError),
}

/// Whether a create, write, delete or rename of `path` is allowed by the composition: the
/// nearest existing node at or above `path` must not be at or below a composed gitlink.
/// Call it from every mutating provider callback before the overlay is touched.
pub async fn check_write(fs: &CoreFs, path: &RepoPath) -> Result<(), WriteError> {
    let mut cur = Some(path.clone());
    while let Some(p) = cur {
        if let Some(node) = fs.resolve(&p).await? {
            return if node.read_only {
                Err(WriteDenied(path.clone()).into())
            } else {
                Ok(())
            };
        }
        cur = p.parent();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use asc_core::codec::Enveloped;
    use asc_core::id::FileId;
    use asc_core::object::{Entry, Kind};
    use asc_core::path::Name;
    use asc_store::bucket::BucketObjects;
    use asc_store::codec::encode_stored;
    use asc_store::key::ObjKey;
    use asc_store::objects::Objects;

    use super::*;
    use crate::core::Options;

    fn rp(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn oid(n: u8) -> GitOid {
        GitOid::from_bytes([n; 20])
    }

    struct Fx {
        store: Arc<BucketObjects>,
    }

    impl Fx {
        fn new() -> Self {
            Self {
                store: Arc::new(BucketObjects::memory()),
            }
        }

        async fn file(&self, name: &str, data: &[u8]) -> Entry {
            let id = FileId::of(data);
            self.store
                .put(&ObjKey::File(id), encode_stored(data))
                .await
                .unwrap();
            Entry {
                name: Name::new(name).unwrap(),
                kind: Kind::File,
                id: *id.as_bytes(),
                size: data.len() as u64,
                git: oid(0),
            }
        }

        async fn tree(&self, mut entries: Vec<Entry>) -> TreeId {
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            let t = Tree::new(entries, true).unwrap();
            self.store
                .put(&ObjKey::Tree(t.id()), encode_stored(&t.to_envelope()))
                .await
                .unwrap();
            t.id()
        }
    }

    fn link(name: &str, commit: GitOid) -> Entry {
        Entry {
            name: Name::new(name).unwrap(),
            kind: Kind::Gitlink,
            id: [0; 32],
            size: 0,
            git: commit,
        }
    }

    /// `(repo, commit)` -> outcome.
    struct Fake(HashMap<(String, GitOid), Result<TreeId, NestedError>>);

    impl Nested for Fake {
        fn root_tree<'a>(
            &'a self,
            repo: &'a str,
            commit: &'a GitOid,
        ) -> BoxFuture<'a, Result<TreeId, NestedError>> {
            let r = self
                .0
                .get(&(repo.to_owned(), *commit))
                .cloned()
                .unwrap_or(Err(NestedError::Unmapped));
            Box::pin(async move { r })
        }
    }

    fn subs(pairs: &[(&str, &str)]) -> Submodules {
        let paths: Vec<RepoPath> = pairs.iter().map(|(p, _)| rp(p)).collect();
        Submodules::resolve("asc/dds", None, &paths, |p| {
            pairs
                .iter()
                .find(|(q, _)| rp(q) == *p)
                .map(|(_, r)| (*r).to_owned())
        })
    }

    #[tokio::test]
    async fn composed_read_denied_write_and_restricted_projection() {
        let fx = Fx::new();
        let lib = fx.file("lib.rs", b"pub fn sdk() {}").await;
        let nested_root = fx.tree(vec![lib]).await;
        let readme = fx.file("README", b"hi").await;
        let root = fx
            .tree(vec![
                readme,
                link("sdk", oid(1)),
                link("secret", oid(2)),
                link("loose", oid(3)),
                link("gone", oid(4)),
            ])
            .await;

        let fake = Fake(HashMap::from([
            (("asc/soma".to_owned(), oid(1)), Ok(nested_root)),
            (("asc/vault".to_owned(), oid(2)), Err(NestedError::Denied)),
        ]));
        let pins = [
            (rp("sdk"), oid(1)),
            (rp("secret"), oid(2)),
            (rp("loose"), oid(3)),
            (rp("gone"), oid(4)),
        ];
        let subs = subs(&[
            ("sdk", "asc/soma"),
            ("secret", "asc/vault"),
            ("gone", "asc/soma"),
        ]);
        let comp = Composition::build(&pins, &subs, &fake).await.unwrap();
        assert_eq!(
            comp.projection(&rp("loose"), &oid(3)),
            Some(&Projection::Unresolved { repo: None })
        );
        assert_eq!(
            comp.projection(&rp("gone"), &oid(4)),
            Some(&Projection::Unresolved {
                repo: Some("asc/soma".into())
            })
        );
        assert_eq!(
            comp.restricted_lines(),
            vec!["secret: restricted (no access to asc/vault)".to_owned()]
        );

        let fs = CoreFs::new(
            fx.store.clone(),
            root,
            Options {
                compose: Some(Arc::new(comp)),
                ..Options::default()
            },
        );

        // A composed read.
        let n = fs.resolve(&rp("sdk/lib.rs")).await.unwrap().unwrap();
        assert!(n.read_only);
        let data = fs
            .read_content(n.content().unwrap())
            .await
            .unwrap()
            .read_all(1024)
            .await
            .unwrap();
        assert_eq!(data, b"pub fn sdk() {}");

        // Denied writes: an existing file, a new file, a new directory, the gitlink itself.
        for p in ["sdk/lib.rs", "sdk/new.rs", "sdk/deep/new.rs", "sdk"] {
            assert!(
                matches!(check_write(&fs, &rp(p)).await, Err(WriteError::Denied(_))),
                "{p}"
            );
        }
        // Outside the composition writes are fine.
        check_write(&fs, &rp("README")).await.unwrap();
        check_write(&fs, &rp("new/file")).await.unwrap();

        // The restricted gitlink is an empty read-only directory.
        let n = fs.resolve(&rp("secret")).await.unwrap().unwrap();
        assert_eq!(n.kind, Kind::Dir);
        assert!(n.read_only);
        let listing = fs.read_dir(Some(&rp("secret"))).await.unwrap().unwrap();
        assert!(listing.entries.is_empty());
        assert!(matches!(
            check_write(&fs, &rp("secret/x")).await,
            Err(WriteError::Denied(_))
        ));

        // Unconfigured and unmapped gitlinks keep the plain empty gitlink.
        for p in ["loose", "gone"] {
            let n = fs.resolve(&rp(p)).await.unwrap().unwrap();
            assert_eq!(n.kind, Kind::Gitlink, "{p}");
            assert!(!n.read_only);
        }
    }

    #[tokio::test]
    async fn a_pin_is_looked_up_by_path_and_commit() {
        let fx = Fx::new();
        let a = fx.file("a", b"1").await;
        let t1 = fx.tree(vec![a]).await;
        let fake = Fake(HashMap::from([(("asc/soma".to_owned(), oid(1)), Ok(t1))]));
        let comp = Composition::build(
            &[(rp("sdk"), oid(1))],
            &subs(&[("sdk", "asc/soma")]),
            &fake,
        )
        .await
        .unwrap();
        assert_eq!(comp.subtree(&rp("sdk"), &oid(1)), Some(t1));
        // Another commit at the same path, or the same commit elsewhere, is not composed.
        assert_eq!(comp.subtree(&rp("sdk"), &oid(9)), None);
        assert_eq!(comp.subtree(&rp("other"), &oid(1)), None);
    }

    #[tokio::test]
    async fn resolver_failures_and_pin_counts_are_bounded() {
        struct Boom;
        impl Nested for Boom {
            fn root_tree<'a>(
                &'a self,
                _: &'a str,
                _: &'a GitOid,
            ) -> BoxFuture<'a, Result<TreeId, NestedError>> {
                Box::pin(async { Err(NestedError::Other("network".into())) })
            }
        }
        let err = Composition::build(
            &[(rp("sdk"), oid(1))],
            &subs(&[("sdk", "asc/soma")]),
            &Boom,
        )
        .await;
        assert!(matches!(err, Err(ComposeError::Nested { .. })));

        let many: Vec<(RepoPath, GitOid)> = (0..=MAX_PINS)
            .map(|i| (rp(&format!("m{i}")), oid(1)))
            .collect();
        let err = Composition::build(&many, &Submodules::default(), &Boom).await;
        assert!(matches!(err, Err(ComposeError::TooMany)));
    }
}
