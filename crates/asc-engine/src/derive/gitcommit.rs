//! `gitcommit.v1`: the git commit id of a changeset (SPEC §5).

use async_trait::async_trait;
use asc_core::git::{ObjectKind, export_commit, object_oid, tree_oid};
use asc_core::id::{ChangesetId, GitOid};
use asc_core::object::Changeset;
use asc_store::objects::Objects;
use asc_store::refs::Refs;

use super::sink::{GitMapRow, GitMapSink};
use super::{Derived, Error, Result};
use crate::Engine;

/// Git commit id of a changeset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GitCommit {
    pub commit: GitOid,
}

#[async_trait]
impl Derived for GitCommit {
    const NAME: &'static str = "gitcommit";
    const VERSION: u32 = 1;
    const EAGER: bool = true;

    /// Raw `git.commit` bytes when present (verified), otherwise the canonical render with
    /// the root tree's git oid and the parents' commits.
    async fn derive<O: Objects, R: Refs>(
        e: &Engine<O, R>,
        cs: &Changeset,
        _: ChangesetId,
        parents: &[Self],
    ) -> Result<Self> {
        let root = e.get_tree(cs.root).await?;
        let parent_gits: Vec<GitOid> = parents.iter().map(|p| p.commit).collect();
        let raw = export_commit(cs, tree_oid(&root), &parent_gits)?;
        Ok(Self {
            commit: object_oid(ObjectKind::Commit, &raw),
        })
    }

    fn encode(&self) -> Vec<u8> {
        self.commit.as_bytes().to_vec()
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let a: [u8; 20] = b
            .try_into()
            .map_err(|_| Error::BadValue { name: Self::NAME })?;
        Ok(Self {
            commit: GitOid::from_bytes(a),
        })
    }
}

/// Emits the git-map row of a changeset (deriving its commit id if needed).
pub async fn index_commit_git<O: Objects, R: Refs>(
    e: &Engine<O, R>,
    id: ChangesetId,
    sink: &dyn GitMapSink,
) -> Result<()> {
    let g = e.get_derived::<GitCommit>(id).await?;
    sink.put(&[GitMapRow {
        git: g.commit,
        kind: ObjectKind::Commit,
        asc: *id.as_bytes(),
    }])
    .await
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use super::*;
    use crate::derive::sink::tests::VecSink;
    use crate::derive::testutil::*;

    /// Runs git in `dir` with a fixed identity, feeding `stdin`; returns trimmed stdout.
    fn git(dir: &Path, args: &[&str], stdin: &[u8]) -> String {
        let mut child = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "A")
            .env("GIT_AUTHOR_EMAIL", "a@x")
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_NAME", "A")
            .env("GIT_COMMITTER_EMAIL", "a@x")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// Writes the asc tree `root` into the git repo at `dir`, returning git's tree id.
    async fn mirror_tree(e: &TestEngine, dir: &Path, root: asc_core::id::TreeId) -> String {
        let tree = e.get_tree(root).await.unwrap();
        let mut listing = String::new();
        for entry in tree.entries() {
            match entry.kind {
                asc_core::object::Kind::File => {
                    let id = asc_core::id::FileId::from_bytes(entry.id);
                    let bytes = e
                        .objects
                        .get(&asc_store::key::ObjKey::File(id))
                        .await
                        .unwrap()
                        .unwrap();
                    let oid = git(dir, &["hash-object", "-w", "--stdin"], &bytes);
                    listing.push_str(&format!("100644 blob {oid}\t{}\n", entry.name));
                }
                asc_core::object::Kind::Dir => {
                    let sub = Box::pin(mirror_tree(
                        e,
                        dir,
                        asc_core::id::TreeId::from_bytes(entry.id),
                    ))
                    .await;
                    listing.push_str(&format!("040000 tree {sub}\t{}\n", entry.name));
                }
                _ => unreachable!("test trees hold files and directories"),
            }
        }
        git(dir, &["mktree"], listing.as_bytes())
    }

    #[tokio::test]
    async fn native_commits_match_git_commit_tree() {
        let e = engine();
        let a = commit_on(&e, &[], b"one").await;
        let b = commit_on(&e, &[a], b"two").await;
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q"], b"");
        let ta = mirror_tree(&e, tmp.path(), e.get_changeset(a).await.unwrap().root).await;
        let tb = mirror_tree(&e, tmp.path(), e.get_changeset(b).await.unwrap().root).await;
        let ca = git(tmp.path(), &["commit-tree", &ta, "-m", "msg"], b"");
        let cb = git(
            tmp.path(),
            &["commit-tree", &tb, "-p", &ca, "-m", "msg"],
            b"",
        );
        assert_eq!(e.git_commit_of(a).await.unwrap().to_string(), ca);
        assert_eq!(e.git_commit_of(b).await.unwrap().to_string(), cb);
    }

    #[tokio::test]
    async fn raw_git_commit_extra_is_used_verbatim() {
        let e = engine();
        let a = commit_on(&e, &[], b"one").await;
        let cs = (*e.get_changeset(a).await.unwrap()).clone();
        let root = e.get_tree(cs.root).await.unwrap();
        let raw = asc_core::git::render_commit(&cs, tree_oid(&root), &[]);
        let mut with_raw = cs.clone();
        let mut extra: Vec<_> = with_raw.extra.as_slice().to_vec();
        extra.push((b"git.commit".to_vec(), raw.clone()));
        with_raw.extra = asc_core::object::SortedMap::new(extra).unwrap();
        let got = GitCommit::derive(&e, &with_raw, a, &[]).await.unwrap();
        assert_eq!(got.commit, object_oid(ObjectKind::Commit, &raw));
    }

    #[tokio::test]
    async fn commit_row_is_emitted() {
        let e = engine();
        let a = commit_on(&e, &[], b"one").await;
        let sink = VecSink::default();
        index_commit_git(&e, a, &sink).await.unwrap();
        let rows = sink.0.lock().unwrap().clone();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, ObjectKind::Commit);
        assert_eq!(rows[0].asc, *a.as_bytes());
        assert_eq!(rows[0].git, e.git_commit_of(a).await.unwrap());
    }
}
