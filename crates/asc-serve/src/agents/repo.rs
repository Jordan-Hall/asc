//! Reading a repository from inside the server: the trunk tip, a tree, a file, the policy.
//!
//! The agent runtime and the workflow engine read through the engine of the repository directly,
//! not through a credential, so that what they need to know (the policy in force, the workflow
//! files of trunk, the base tree of a run) never depends on what the caller may read. They read
//! only; every write goes through the public services with the run's own token.

use std::collections::HashMap;
use std::sync::Arc;

use asc_core::id::{ChangesetId, FileId, RefTarget, TreeId};
use asc_core::object::Kind;
use asc_core::path::{RefName, RepoPath};
use asc_core::policy::{POLICY_PATH, Policy};
use asc_store::refs::Refs as _;
use bytes::Bytes;
use tonic::Status;

use crate::registry::{Registry, RepoEntry};

/// A regular file of a tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeFile {
    /// `/`-separated, no leading slash.
    pub path: String,
    pub id: FileId,
    pub size: u64,
    pub exec: bool,
}

/// An entry of one directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
}

/// What the policy file of a repository is.
#[derive(Clone, Debug)]
pub enum PolicyState {
    /// No file: no policy binds anyone.
    Absent,
    Valid(Arc<Policy>),
    /// A file that is there and cannot be used. Agents fail closed on it.
    Invalid(String),
}

/// The tree walked past a bound.
#[derive(Debug, PartialEq, Eq)]
pub enum WalkError {
    TooManyFiles,
    TooManyBytes,
    Store(String),
}

/// A changeset id from its 32 bytes.
pub fn changeset_id(b: &[u8]) -> Option<ChangesetId> {
    <[u8; 32]>::try_from(b).ok().map(ChangesetId::from_bytes)
}

fn internal(context: &str, e: &dyn std::fmt::Display) -> Status {
    crate::update::internal(context, e)
}

/// The reader.
pub struct RepoView {
    registry: Arc<Registry>,
    policies: quick_cache::sync::Cache<(i64, ChangesetId), Arc<PolicyState>>,
}

impl RepoView {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self {
            registry,
            policies: quick_cache::sync::Cache::new(256),
        }
    }

    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// The changeset a branch (`refs/heads/<name>` or any full ref name) points at.
    pub async fn ref_tip(
        &self,
        entry: &RepoEntry,
        full_name: &str,
    ) -> Result<Option<ChangesetId>, Status> {
        let name = RefName::new(full_name.as_bytes())
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let got = self
            .registry
            .refs()
            .get(entry.key, &name)
            .await
            .map_err(|e| internal("repo view: refs", &e))?;
        Ok(match got.map(|r| r.target) {
            Some(RefTarget::Changeset(c)) => Some(c),
            _ => None,
        })
    }

    /// The tip of trunk, the default branch.
    pub async fn trunk_tip(&self, entry: &RepoEntry) -> Result<Option<ChangesetId>, Status> {
        self.ref_tip(entry, &format!("refs/heads/{}", entry.default_branch))
            .await
    }

    /// The root tree of a changeset.
    pub async fn root_of(&self, entry: &RepoEntry, cs: ChangesetId) -> Result<TreeId, Status> {
        Ok(entry
            .backend
            .engine()
            .get_changeset(cs)
            .await
            .map_err(|e| internal("repo view: changeset", &e))?
            .root)
    }

    /// The bytes of the regular file `path` under `root`, or `None` when there is no such file.
    /// A file larger than `max` is an error, never a prefix.
    pub async fn read(
        &self,
        entry: &RepoEntry,
        root: TreeId,
        path: &str,
        max: u64,
    ) -> Result<Option<Bytes>, Status> {
        let Ok(p) = RepoPath::new(path.as_bytes()) else {
            return Ok(None);
        };
        let engine = entry.backend.engine();
        let Some(e) = engine
            .lookup(root, &p)
            .await
            .map_err(|e| internal("repo view: lookup", &e))?
        else {
            return Ok(None);
        };
        if !matches!(e.kind, Kind::File | Kind::Exec) {
            return Ok(None);
        }
        if e.size > max {
            return Err(Status::failed_precondition(format!(
                "{path} is larger than {max} bytes"
            )));
        }
        engine
            .read_file(FileId::from_bytes(e.id), e.size)
            .await
            .map(Some)
            .map_err(|e| internal("repo view: read", &e))
    }

    /// The entries of directory `dir` under `root` (`""` is the root); `None` when it is no directory.
    pub async fn list_dir(
        &self,
        entry: &RepoEntry,
        root: TreeId,
        dir: &str,
    ) -> Result<Option<Vec<DirEntry>>, Status> {
        let engine = entry.backend.engine();
        let tree = if dir.is_empty() {
            root
        } else {
            let Ok(p) = RepoPath::new(dir.as_bytes()) else {
                return Ok(None);
            };
            match engine
                .lookup(root, &p)
                .await
                .map_err(|e| internal("repo view: lookup", &e))?
            {
                Some(e) if e.kind == Kind::Dir => TreeId::from_bytes(e.id),
                _ => return Ok(None),
            }
        };
        let t = engine
            .get_tree(tree)
            .await
            .map_err(|e| internal("repo view: tree", &e))?;
        Ok(Some(
            t.entries()
                .iter()
                .map(|e| DirEntry {
                    name: String::from_utf8_lossy(e.name.as_bytes()).into_owned(),
                    dir: e.kind == Kind::Dir,
                    size: e.size,
                })
                .collect(),
        ))
    }

    /// Every regular file under `root`, sorted by path, within the bounds. Symlinks and
    /// gitlinks are left out (a working copy has neither).
    pub async fn walk(
        &self,
        entry: &RepoEntry,
        root: TreeId,
        max_files: usize,
        max_bytes: u64,
    ) -> Result<Vec<TreeFile>, WalkError> {
        let engine = entry.backend.engine();
        let mut out: Vec<TreeFile> = Vec::new();
        let mut bytes = 0u64;
        let mut stack: Vec<(String, TreeId)> = vec![(String::new(), root)];
        while let Some((prefix, tree)) = stack.pop() {
            let t = engine
                .get_tree(tree)
                .await
                .map_err(|e| WalkError::Store(e.to_string()))?;
            for e in t.entries() {
                let name = String::from_utf8_lossy(e.name.as_bytes()).into_owned();
                let path = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                match e.kind {
                    Kind::Dir => stack.push((path, TreeId::from_bytes(e.id))),
                    Kind::File | Kind::Exec => {
                        bytes = bytes.saturating_add(e.size);
                        if out.len() >= max_files {
                            return Err(WalkError::TooManyFiles);
                        }
                        if bytes > max_bytes {
                            return Err(WalkError::TooManyBytes);
                        }
                        out.push(TreeFile {
                            path,
                            id: FileId::from_bytes(e.id),
                            size: e.size,
                            exec: e.kind == Kind::Exec,
                        });
                    }
                    Kind::Symlink | Kind::Gitlink => {}
                }
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// The policy in force: the file on the trunk tip.
    pub async fn policy(&self, entry: &RepoEntry) -> Result<Arc<PolicyState>, Status> {
        let Some(tip) = self.trunk_tip(entry).await? else {
            return Ok(Arc::new(PolicyState::Absent));
        };
        self.policy_at(entry, tip).await
    }

    /// The policy file of changeset `cs`.
    pub async fn policy_at(
        &self,
        entry: &RepoEntry,
        cs: ChangesetId,
    ) -> Result<Arc<PolicyState>, Status> {
        if let Some(hit) = self.policies.get(&(entry.key.0, cs)) {
            return Ok(hit);
        }
        let root = self.root_of(entry, cs).await?;
        let state = match self
            .read(
                entry,
                root,
                POLICY_PATH,
                asc_core::policy::MAX_POLICY_BYTES as u64,
            )
            .await
        {
            Ok(None) => PolicyState::Absent,
            Ok(Some(text)) => match Policy::parse(&text) {
                Ok(p) => PolicyState::Valid(Arc::new(p)),
                Err(e) => PolicyState::Invalid(e.0),
            },
            // Too large is a file that cannot be used; a store failure is an error.
            Err(s) if s.code() == tonic::Code::FailedPrecondition => {
                PolicyState::Invalid(s.message().to_owned())
            }
            Err(s) => return Err(s),
        };
        let state = Arc::new(state);
        self.policies.insert((entry.key.0, cs), state.clone());
        Ok(state)
    }

    /// The ids of the changesets between `base` and `tip`, newest first, at most `limit`;
    /// `None` when there are more.
    pub async fn range(
        &self,
        entry: &RepoEntry,
        tip: ChangesetId,
        exclude: Vec<ChangesetId>,
        limit: usize,
    ) -> Result<Option<Vec<ChangesetId>>, Status> {
        use futures::TryStreamExt as _;
        let graph = asc_engine::graph::Graph::new(crate::graph_sql::SqlGraph::new(
            self.registry.pool().clone(),
            entry.ns,
            entry.backend.engine().clone(),
        ));
        let ids: Vec<ChangesetId> = graph
            .log(asc_engine::graph::LogQuery {
                include: vec![tip],
                exclude,
                limit: Some(limit + 1),
                ..asc_engine::graph::LogQuery::default()
            })
            .try_collect()
            .await
            .map_err(|e| internal("repo view: history", &e))?;
        Ok((ids.len() <= limit).then_some(ids))
    }

    /// The paths a changeset changes against its first parent.
    pub async fn changed_paths(
        &self,
        entry: &RepoEntry,
        cs: ChangesetId,
    ) -> Result<Vec<String>, Status> {
        let body = entry
            .backend
            .engine()
            .get_changeset(cs)
            .await
            .map_err(|e| internal("repo view: changeset", &e))?;
        Ok(body
            .changes
            .as_slice()
            .iter()
            .map(|(p, _)| String::from_utf8_lossy(p.as_bytes()).into_owned())
            .collect())
    }

    /// Reads many files into a map, for tools that want a whole small directory.
    pub async fn read_many(
        &self,
        entry: &RepoEntry,
        files: &[TreeFile],
        max_each: u64,
    ) -> Result<HashMap<String, Bytes>, Status> {
        let engine = entry.backend.engine();
        let mut out = HashMap::with_capacity(files.len());
        for f in files {
            if f.size > max_each {
                continue;
            }
            let b = engine
                .read_file(f.id, f.size)
                .await
                .map_err(|e| internal("repo view: read", &e))?;
            out.insert(f.path.clone(), b);
        }
        Ok(out)
    }
}
