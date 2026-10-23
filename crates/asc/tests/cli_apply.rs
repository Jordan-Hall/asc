//! `apply` through the built binary: a patch that changes where a symbolic link points is held
//! to the same rules as one that creates it, whatever headers the patch has. The staged link is
//! built with the fixture (no sibling command makes one), and `--cached` applies to it.

mod support;

use std::fs;
use std::io::Cursor;

use asc_core::content::{Body, ingest};
use asc_core::object::Kind;
use asc_state::state::stage::{self, Leaf, StageRow, StageSet};
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use support::{Fixture, text};

/// A workspace on `master` with `f.txt`, and a staged symbolic link `link -> target`.
async fn with_link() -> Fixture {
    let f = Fixture::new().await;
    let a = f.commit(&[], &[("f.txt", b"one\n")]).await;
    f.set_branch("master", a).await;
    f.checkout_branch("master", Some(a)).await;
    fs::write(f.root.join("f.txt"), "one\n").unwrap();
    let got = ingest(Cursor::new(&b"target"[..])).unwrap();
    let Body::Inline(bytes) = got.body else {
        panic!("a link target is small");
    };
    f.objects
        .put(&ObjKey::File(got.file), encode_stored(&bytes))
        .await
        .unwrap();
    let leaf = Leaf {
        kind: Kind::Symlink,
        file: got.file,
        size: got.size,
        git: got.git,
    };
    f.stage(&[StageRow::from_leaf(b"link".to_vec(), Some(&leaf))])
        .await;
    f
}

/// A patch with no mode header that rewrites the target of `link`.
fn retarget(to: &str) -> String {
    format!(
        "--- a/link\n+++ b/link\n@@ -1 +1 @@\n-target\n\\ No newline at end of file\n+{to}\n\\ No newline at end of file\n"
    )
}

async fn staged_link(f: &Fixture) -> StageRow {
    stage::overlay(&f.db, f.ws, StageSet::Default)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.path == b"link")
        .expect("link is staged")
}

#[tokio::test]
async fn a_content_patch_cannot_point_a_staged_link_outside_the_tree_or_at_dotgit() {
    if support::asc_binary().is_none() {
        return;
    }
    let f = with_link().await;
    let before = staged_link(&f).await;
    for to in [
        "../../etc",
        "../outside",
        "/etc/passwd",
        ".git/hooks",
        "d/.git /hooks",
        "GIT~1",
        ".asc/info/exclude",
    ] {
        fs::write(f.root.join("l.patch"), retarget(to)).unwrap();
        let out = f
            .asc()
            .args(["apply", "--cached", "l.patch"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{to}: {}", text(&out.stderr));
        assert!(
            text(&out.stderr).contains("invalid path"),
            "{to}: {}",
            text(&out.stderr)
        );
        assert_eq!(staged_link(&f).await, before, "{to}: the link changed");
    }
}

#[tokio::test]
async fn a_content_patch_may_retarget_a_staged_link_inside_the_tree() {
    if support::asc_binary().is_none() {
        return;
    }
    let f = with_link().await;
    let before = staged_link(&f).await;
    fs::write(f.root.join("l.patch"), retarget("sub/other")).unwrap();
    let out = f
        .asc()
        .args(["apply", "--cached", "l.patch"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let after = staged_link(&f).await;
    assert_eq!(after.kind, Kind::Symlink);
    assert_ne!(after.git, before.git);
}
