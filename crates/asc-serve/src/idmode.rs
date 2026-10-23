//! Id translation for `id_mode = GIT` (SPEC §7, §8.2).
//!
//! In git mode a client speaks 20-byte git oids and the server keeps asc ids. Inbound oids are
//! looked up in `git_map`; outbound oids need no lookup: a tree entry carries its git oid, a tree
//! hashes to its own, and a changeset's commit oid is a memoised derived value.

use std::collections::{HashMap, hash_map};
use std::sync::Arc;

use bytes::Bytes;
use asc_core::git::{ObjectKind, tree_oid};
use asc_core::id::{ChangesetId, FileId, GitOid, TreeId};
use asc_core::object::Entry;
use asc_engine::Engine;
use asc_engine::revspec::{Peel, RevExpr};
use asc_rpc::IdMode;
use asc_rpc::convert::WireId;
use asc_store::bucket::BucketObjects;
use asc_store::refs::{Refs, RepoKey};
use asc_store::sqlrefs::SqlRefs;
use futures::stream::{self, StreamExt, TryStreamExt};
use sqlx::{AnyPool, Row};
use tonic::Status;

use crate::registry::{Ns, Registry, RepoEntry};

/// The engine type the server runs per repository.
pub type Eng = Engine<BucketObjects, SqlRefs>;

/// Most git oids looked up in one SQL statement.
const LOOKUP_BATCH: usize = 200;
/// Concurrent commit-oid derivations in one batch.
const DERIVE_FANOUT: usize = 16;

/// `git_map.kind` code of an object kind (the column [`GitMapSink`](crate::registry::SqlGitMap) writes).
pub fn kind_code(k: ObjectKind) -> i16 {
    match k {
        ObjectKind::Blob => 1,
        ObjectKind::Tree => 2,
        ObjectKind::Commit => 3,
        ObjectKind::Tag => 4,
    }
}

/// Translates the ids of one call between the client's id space and asc ids.
#[derive(Clone)]
pub struct IdCodec {
    mode: IdMode,
    pool: AnyPool,
    ns: Ns,
    repo: RepoKey,
    refs: Arc<SqlRefs>,
    engine: Arc<Eng>,
}

fn internal(context: &str, e: &dyn std::fmt::Display) -> Status {
    tracing::error!(%context, error = %e, "internal error");
    Status::internal("internal error")
}

fn is_hex(s: &[u8]) -> bool {
    !s.is_empty() && s.iter().all(u8::is_ascii_hexdigit)
}

/// A name inside a parsed revision. `rev` names a commit (hex ids and `HEAD` are meaningful);
/// otherwise it is a ref name only (`name@{u}`, `name@{n}`).
struct Leaf {
    name: String,
    rev: bool,
}

/// Collects every name of `e`; refuses `:/regex` message searches.
fn collect_leaves(e: &RevExpr, out: &mut Vec<Leaf>) -> Result<(), Status> {
    match e {
        RevExpr::Name(n) => out.push(Leaf {
            name: n.clone(),
            rev: true,
        }),
        RevExpr::Upstream(Some(n)) | RevExpr::Reflog(Some(n), _) => out.push(Leaf {
            name: n.clone(),
            rev: false,
        }),
        RevExpr::Upstream(None) | RevExpr::Reflog(None, _) => {}
        RevExpr::Message(_) => {
            return Err(Status::invalid_argument(
                "message searches are not supported",
            ));
        }
        RevExpr::Parent(x, _)
        | RevExpr::Ancestor(x, _)
        | RevExpr::Peel(x, _)
        | RevExpr::Path(x, _)
        | RevExpr::Not(x)
        | RevExpr::Parents(x)
        | RevExpr::Only(x) => collect_leaves(x, out)?,
        RevExpr::DotDot(a, b) | RevExpr::TriDot(a, b) => {
            collect_leaves(a, out)?;
            collect_leaves(b, out)?;
        }
        RevExpr::List(xs) => {
            for x in xs {
                collect_leaves(x, out)?;
            }
        }
    }
    Ok(())
}

/// Writes `e` as a spec, with each name replaced by its entry in `names`.
fn render(e: &RevExpr, names: &HashMap<(String, bool), String>, out: &mut String) {
    use std::fmt::Write as _;
    let name = |n: &String, rev: bool| names.get(&(n.clone(), rev)).unwrap_or(n).clone();
    match e {
        RevExpr::Name(n) => out.push_str(&name(n, true)),
        RevExpr::Parent(x, n) => {
            render(x, names, out);
            out.push('^');
            if *n != 1 {
                let _ = write!(out, "{n}");
            }
        }
        RevExpr::Ancestor(x, n) => {
            render(x, names, out);
            let _ = write!(out, "~{n}");
        }
        RevExpr::Peel(x, p) => {
            render(x, names, out);
            out.push_str(match p {
                Peel::Tree => "^{tree}",
                Peel::Commit => "^{commit}",
            });
        }
        RevExpr::Upstream(n) => {
            if let Some(n) = n {
                out.push_str(&name(n, false));
            }
            out.push_str("@{u}");
        }
        RevExpr::Reflog(n, k) => {
            if let Some(n) = n {
                out.push_str(&name(n, false));
            }
            let _ = write!(out, "@{{{k}}}");
        }
        // Refused by `collect_leaves` before rendering.
        RevExpr::Message(re) => {
            out.push_str(":/");
            out.push_str(re);
        }
        RevExpr::Path(x, p) => {
            render(x, names, out);
            let _ = write!(out, ":{p}");
        }
        RevExpr::DotDot(a, b) => {
            render(a, names, out);
            out.push_str("..");
            render(b, names, out);
        }
        RevExpr::TriDot(a, b) => {
            render(a, names, out);
            out.push_str("...");
            render(b, names, out);
        }
        RevExpr::Not(x) => {
            out.push('^');
            render(x, names, out);
        }
        RevExpr::Parents(x) => {
            render(x, names, out);
            out.push_str("^@");
        }
        RevExpr::Only(x) => {
            render(x, names, out);
            out.push_str("^!");
        }
        RevExpr::List(xs) => {
            for (i, x) in xs.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                render(x, names, out);
            }
        }
    }
}

impl IdCodec {
    /// A codec for a call on `entry` in `mode`.
    pub fn new(mode: IdMode, registry: &Registry, entry: &RepoEntry) -> Self {
        Self {
            mode,
            pool: registry.pool().clone(),
            ns: entry.ns,
            repo: entry.key,
            refs: registry.refs().clone(),
            engine: entry.backend.engine().clone(),
        }
    }

    pub fn mode(&self) -> IdMode {
        self.mode
    }

    /// The asc ids of `git` oids of `kind`, in order; `None` for an oid the map does not know.
    pub async fn asc_of_git(
        &self,
        kind: ObjectKind,
        git: &[GitOid],
    ) -> Result<Vec<Option<[u8; 32]>>, Status> {
        let mut out = Vec::with_capacity(git.len());
        for chunk in git.chunks(LOOKUP_BATCH) {
            let marks = (0..chunk.len())
                .map(|i| format!("${}", i + 3))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT git, asc FROM git_map WHERE ns = $1 AND kind = $2 AND git IN ({marks})"
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(self.ns.as_bytes().to_vec())
                .bind(i32::from(kind_code(kind)));
            for g in chunk {
                q = q.bind(g.as_bytes().to_vec());
            }
            let rows = q
                .fetch_all(&self.pool)
                .await
                .map_err(|e| internal("git_map", &e))?;
            let mut found = std::collections::HashMap::with_capacity(rows.len());
            for r in rows {
                let g: Vec<u8> = r.try_get("git").map_err(|e| internal("git_map", &e))?;
                let c: Vec<u8> = r.try_get("asc").map_err(|e| internal("git_map", &e))?;
                if let Ok(c) = <[u8; 32]>::try_from(c.as_slice()) {
                    found.insert(g, c);
                }
            }
            out.extend(
                chunk
                    .iter()
                    .map(|g| found.get(g.as_bytes().as_slice()).copied()),
            );
        }
        Ok(out)
    }

    /// The changeset a wire id names. A git oid must be a commit in the map.
    pub async fn changeset_in(&self, wire: &[u8]) -> Result<ChangesetId, Status> {
        match self.mode {
            IdMode::Asc => {
                ChangesetId::from_slice(wire).map_err(|e| Status::invalid_argument(e.to_string()))
            }
            IdMode::Git => {
                let g = GitOid::from_slice(wire)
                    .map_err(|e| Status::invalid_argument(e.to_string()))?;
                let got = self.asc_of_git(ObjectKind::Commit, &[g]).await?;
                got.into_iter()
                    .next()
                    .flatten()
                    .map(ChangesetId::from_bytes)
                    .ok_or_else(|| Status::not_found(format!("unknown commit {g}")))
            }
        }
    }

    /// The file a wire id names. A git oid must be a blob in the map.
    pub async fn file_in(&self, wire: &[u8]) -> Result<FileId, Status> {
        match self.mode {
            IdMode::Asc => {
                FileId::from_slice(wire).map_err(|e| Status::invalid_argument(e.to_string()))
            }
            IdMode::Git => {
                let g = GitOid::from_slice(wire)
                    .map_err(|e| Status::invalid_argument(e.to_string()))?;
                let got = self.asc_of_git(ObjectKind::Blob, &[g]).await?;
                got.into_iter()
                    .next()
                    .flatten()
                    .map(FileId::from_bytes)
                    .ok_or_else(|| Status::not_found(format!("unknown blob {g}")))
            }
        }
    }

    /// The tree a wire id names. A git oid must be a tree in the map.
    pub async fn tree_in(&self, wire: &[u8]) -> Result<TreeId, Status> {
        match self.mode {
            IdMode::Asc => {
                TreeId::from_slice(wire).map_err(|e| Status::invalid_argument(e.to_string()))
            }
            IdMode::Git => {
                let g = GitOid::from_slice(wire)
                    .map_err(|e| Status::invalid_argument(e.to_string()))?;
                let got = self.asc_of_git(ObjectKind::Tree, &[g]).await?;
                got.into_iter()
                    .next()
                    .flatten()
                    .map(TreeId::from_bytes)
                    .ok_or_else(|| Status::not_found(format!("unknown tree {g}")))
            }
        }
    }

    /// Rewrites the revision `spec` into the asc id space.
    ///
    /// The spec is parsed with the engine's parser and every name in it is checked, so the
    /// policy holds for `A..B`, `A...B`, `rev:path`, `name@{n}` and the like, not only for a
    /// leading token. A 40-digit hex name is a git commit oid in git mode and becomes the asc id
    /// it maps to, so `<oid>~2` and `<oid>^{tree}` work. `HEAD` and `@` name the default branch.
    /// Other hex-only names of 4 to 63 digits are refused unless a ref has that name (an
    /// abbreviation lookup would scan the whole repository). `:/regex` message searches walk
    /// the whole history and are refused. What the engine resolves is the re-rendered parse
    /// tree, so it cannot differ from what was checked.
    pub async fn rev_in(&self, spec: &str, default_branch: &str) -> Result<String, Status> {
        let expr = asc_engine::revspec::parse(spec)
            .map_err(|e| Status::invalid_argument(format!("bad revision: {e}")))?;
        let mut leaves = Vec::new();
        collect_leaves(&expr, &mut leaves)?;
        let mut names = HashMap::new();
        for leaf in leaves {
            let key = (leaf.name.clone(), leaf.rev);
            if let hash_map::Entry::Vacant(slot) = names.entry(key) {
                slot.insert(self.name_in(&leaf, default_branch).await?);
            }
        }
        let mut out = String::with_capacity(spec.len() + 8);
        render(&expr, &names, &mut out);
        Ok(out)
    }

    /// The asc-space spelling of one name of a revision.
    async fn name_in(&self, leaf: &Leaf, default_branch: &str) -> Result<String, Status> {
        let head = leaf.name.as_str();
        if head == "HEAD" || head == "@" {
            return Ok(format!("refs/heads/{default_branch}"));
        }
        if !is_hex(head.as_bytes()) || head.len() < 4 || (head.len() == 64 && leaf.rev) {
            return Ok(head.to_owned());
        }
        if leaf.rev && head.len() == 40 && self.mode == IdMode::Git {
            let mut raw = [0u8; 20];
            for (i, b) in raw.iter_mut().enumerate() {
                *b = u8::from_str_radix(&head[2 * i..2 * i + 2], 16)
                    .map_err(|_| Status::invalid_argument("bad revision"))?;
            }
            return Ok(self.changeset_in(&raw).await?.to_string());
        }
        // A hex-looking name: only an existing ref may use it.
        for cand in asc_engine::revspec::dwim_candidates(head) {
            let Ok(name) = asc_core::path::RefName::new_onelevel(cand) else {
                continue;
            };
            let found = self
                .refs
                .get(self.repo, &name)
                .await
                .map_err(|e| internal("refs", &e))?;
            if found.is_some() {
                return Ok(head.to_owned());
            }
        }
        Err(Status::invalid_argument(
            "abbreviated ids are not supported; use a full id or a ref name",
        ))
    }

    /// The wire form of a changeset id.
    pub async fn changeset_out(&self, id: ChangesetId) -> Result<Bytes, Status> {
        match self.mode {
            IdMode::Asc => Ok(Bytes::copy_from_slice(id.as_bytes())),
            IdMode::Git => {
                let g = self
                    .engine
                    .git_commit_of(id)
                    .await
                    .map_err(|e| internal("git commit", &e))?;
                Ok(Bytes::copy_from_slice(g.as_bytes()))
            }
        }
    }

    /// The git commit oid of a changeset (both modes).
    pub async fn git_of(&self, id: ChangesetId) -> Result<GitOid, Status> {
        self.engine
            .git_commit_of(id)
            .await
            .map_err(|e| internal("git commit", &e))
    }

    /// Wire forms of `ids`, in order; derivations run a few at a time.
    pub async fn changesets_out(&self, ids: &[ChangesetId]) -> Result<Vec<Bytes>, Status> {
        stream::iter(ids.iter().copied())
            .map(|id| async move { self.changeset_out(id).await })
            .buffered(DERIVE_FANOUT)
            .try_collect()
            .await
    }

    /// The wire form of a tree whose git oid is `git`.
    pub fn tree_out(&self, id: TreeId, git: GitOid) -> Bytes {
        match self.mode {
            IdMode::Asc => Bytes::copy_from_slice(id.as_bytes()),
            IdMode::Git => Bytes::copy_from_slice(git.as_bytes()),
        }
    }

    /// The git oid of a loaded tree.
    pub async fn tree_git(&self, id: TreeId) -> Result<GitOid, Status> {
        let tree = self
            .engine
            .get_tree(id)
            .await
            .map_err(|e| internal("tree", &e))?;
        Ok(tree_oid(&tree))
    }

    /// The wire id of a tree entry (a file's, directory's or gitlink's).
    pub fn entry_out(&self, e: &Entry) -> Bytes {
        match self.mode {
            IdMode::Asc if e.kind == asc_core::object::Kind::Gitlink => Bytes::new(),
            IdMode::Asc => Bytes::copy_from_slice(&e.id),
            IdMode::Git => Bytes::copy_from_slice(e.git.as_bytes()),
        }
    }
}

#[cfg(test)]
mod tests {
    use asc_core::git::blob_oid;
    use asc_core::object::Kind;
    use asc_engine::derive::{GitMapRow, GitMapSink};

    use super::*;
    use crate::ingest::testutil::{commit, registry, repo};

    async fn setup() -> (Arc<crate::registry::Registry>, Arc<RepoEntry>, ChangesetId) {
        let r = registry().await;
        let e = repo(&r).await;
        let c = commit(&e, &[], &[("a.txt", b"hello\n"), ("d/b.txt", b"x\n")]).await;
        let git = e.backend.engine().git_commit_of(c).await.unwrap();
        e.git_map
            .put(&[GitMapRow {
                git,
                kind: ObjectKind::Commit,
                asc: *c.as_bytes(),
            }])
            .await
            .unwrap();
        let root = e.backend.engine().get_changeset(c).await.unwrap().root;
        asc_engine::derive::index_tree_git(e.backend.engine(), root, &*e.git_map)
            .await
            .unwrap();
        (r, e, c)
    }

    #[tokio::test]
    async fn git_oids_map_both_ways() {
        let (r, e, c) = setup().await;
        let git = IdCodec::new(IdMode::Git, &r, &e);
        let g = git.git_of(c).await.unwrap();
        assert_eq!(git.changeset_in(g.as_bytes()).await.unwrap(), c);
        assert_eq!(&git.changeset_out(c).await.unwrap()[..], g.as_bytes());
        // A blob oid resolves to its file id and a commit oid is not a blob.
        let blob = blob_oid(b"hello\n");
        assert_eq!(
            git.file_in(blob.as_bytes()).await.unwrap(),
            FileId::of(b"hello\n")
        );
        assert_eq!(
            git.file_in(g.as_bytes()).await.unwrap_err().code(),
            tonic::Code::NotFound
        );
        // Wrong lengths are rejected in both modes.
        assert!(git.changeset_in(&[0; 32]).await.is_err());
        let asc = IdCodec::new(IdMode::Asc, &r, &e);
        assert!(asc.changeset_in(&[0; 20]).await.is_err());
        assert_eq!(asc.changeset_in(c.as_bytes()).await.unwrap(), c);
    }

    #[tokio::test]
    async fn namespaces_do_not_leak() {
        let (r, _e, c) = setup().await;
        let other = r
            .create(
                &asc_rpc::server::RepoName::parse("o/other").unwrap(),
                &Default::default(),
            )
            .await
            .unwrap();
        let git = IdCodec::new(IdMode::Git, &r, &other);
        let g = other.backend.engine().git_commit_of(c).await;
        // The other namespace has neither the changeset nor a map row for it.
        assert!(g.is_err());
        assert_eq!(
            git.changeset_in(&[7; 20]).await.unwrap_err().code(),
            tonic::Code::NotFound
        );
    }

    #[tokio::test]
    async fn revisions_are_rewritten_into_asc_space() {
        let (r, e, c) = setup().await;
        let git = IdCodec::new(IdMode::Git, &r, &e);
        let g = git.git_of(c).await.unwrap();
        assert_eq!(
            git.rev_in(&g.to_string(), "master").await.unwrap(),
            c.to_string()
        );
        assert_eq!(
            git.rev_in(&format!("{g}~2"), "master").await.unwrap(),
            format!("{c}~2")
        );
        assert_eq!(
            git.rev_in("HEAD", "master").await.unwrap(),
            "refs/heads/master"
        );
        assert_eq!(
            git.rev_in("HEAD~1", "trunk").await.unwrap(),
            "refs/heads/trunk~1"
        );
        assert_eq!(git.rev_in("topic", "master").await.unwrap(), "topic");
        assert_eq!(
            git.rev_in(&format!("{c}^"), "master").await.unwrap(),
            format!("{c}^")
        );
        // An unmapped oid is not found; an abbreviation is refused rather than scanned for.
        assert_eq!(
            git.rev_in(&"ab".repeat(20), "master")
                .await
                .unwrap_err()
                .code(),
            tonic::Code::NotFound
        );
        assert_eq!(
            git.rev_in("abcdef1", "master").await.unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
    }

    #[tokio::test]
    async fn abbreviations_and_searches_are_refused_anywhere_in_a_spec() {
        let (r, e, c) = setup().await;
        let g = IdCodec::new(IdMode::Git, &r, &e).git_of(c).await.unwrap();
        for mode in [IdMode::Git, IdMode::Asc] {
            let codec = IdCodec::new(mode, &r, &e);
            for spec in [
                "master..abcd1234",
                "abcd1234..master",
                "HEAD~1...beef",
                "abcd1234:a.txt",
                "abcd1234@{1}",
                "abcd1234@{u}",
                "abcd1234^{tree}",
                "abcd1234^@",
                "^abcd1234",
                "abcd^{/regex}",
                "master^{/regex}",
                ":/regex",
                "HEAD~1^{/x}",
                "x@{...}",
                "master..:/regex",
            ] {
                let err = codec
                    .rev_in(spec, "master")
                    .await
                    .expect_err(&format!("{mode:?} {spec} must be refused"));
                assert_eq!(err.code(), tonic::Code::InvalidArgument, "{spec}");
            }
        }
        // A full oid is still translated wherever it stands, and ordinary specs pass.
        let git = IdCodec::new(IdMode::Git, &r, &e);
        assert_eq!(
            git.rev_in(&format!("{g}~2"), "master").await.unwrap(),
            format!("{c}~2")
        );
        assert_eq!(
            git.rev_in(&format!("master..{g}"), "master").await.unwrap(),
            format!("master..{c}")
        );
        assert_eq!(
            git.rev_in(&format!("{g}^{{tree}}"), "master")
                .await
                .unwrap(),
            format!("{c}^{{tree}}")
        );
        assert_eq!(
            git.rev_in("HEAD...topic", "trunk").await.unwrap(),
            "refs/heads/trunk...topic"
        );
        assert_eq!(
            git.rev_in(&format!("{g}:a.txt"), "master").await.unwrap(),
            format!("{c}:a.txt")
        );
    }

    #[tokio::test]
    async fn entries_carry_their_own_git_oid() {
        let (r, e, c) = setup().await;
        let root = e.backend.engine().get_changeset(c).await.unwrap().root;
        let tree = e.backend.engine().get_tree(root).await.unwrap();
        let file = tree
            .entries()
            .iter()
            .find(|x| x.kind == Kind::File)
            .unwrap();
        let git = IdCodec::new(IdMode::Git, &r, &e);
        let asc = IdCodec::new(IdMode::Asc, &r, &e);
        assert_eq!(&git.entry_out(file)[..], file.git.as_bytes());
        assert_eq!(&asc.entry_out(file)[..], &file.id[..]);
        let g = git.tree_git(root).await.unwrap();
        assert_eq!(&git.tree_out(root, g)[..], g.as_bytes());
        assert_eq!(git.tree_in(g.as_bytes()).await.unwrap(), root);
    }
}
