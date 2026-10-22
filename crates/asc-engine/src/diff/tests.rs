//! Behaviour tests. The scenario in `tests/diff_golden/scenario` is committed twice (c1, c2);
//! `scenario.sh` lets git render every output form, and these tests build the same two trees in an
//! engine and require the same bytes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::codec::Enveloped;
use asc_core::content::{Body, ingest};
use asc_core::id::TreeId;
use asc_core::object::{Change, Kind};
use asc_core::path::{RefName, RepoPath};
use asc_store::bucket::BucketObjects;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::{RefEntry, RefError, RefLogEntry, RefTxn, Refs, RepoKey};

use super::format::{write_name_only, write_name_status, write_raw, write_summary};
use super::*;

struct NoRefs;

#[async_trait]
impl Refs for NoRefs {
    async fn get(&self, _: RepoKey, _: &RefName) -> asc_store::refs::Result<Option<RefEntry>> {
        Ok(None)
    }
    async fn list(&self, _: RepoKey, _: &str) -> asc_store::refs::Result<Vec<RefEntry>> {
        Ok(Vec::new())
    }
    async fn update(&self, _: RepoKey, _: &RefTxn) -> asc_store::refs::Result<()> {
        Err(RefError::Corrupt("unused".into()))
    }
    async fn log(
        &self,
        _: RepoKey,
        _: &RefName,
        _: u32,
    ) -> asc_store::refs::Result<Vec<RefLogEntry>> {
        Ok(Vec::new())
    }
}

type TestEngine = Engine<BucketObjects, NoRefs>;

fn engine() -> TestEngine {
    Engine::new(
        Arc::new(BucketObjects::memory()),
        Arc::new(NoRefs),
        RepoKey(1),
    )
}

fn p(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap()
}

async fn put_file(e: &TestEngine, kind: Kind, content: &[u8]) -> Change {
    let got = ingest(std::io::Cursor::new(content)).unwrap();
    match got.body {
        Body::Inline(b) => {
            e.objects
                .put(&ObjKey::File(got.file), encode_stored(&b))
                .await
                .unwrap();
        }
        Body::Chunked { list, chunks } => {
            for (id, data) in chunks {
                e.objects
                    .put(&ObjKey::Chunk(id), encode_stored(&data))
                    .await
                    .unwrap();
            }
            e.objects
                .put(
                    &ObjKey::ChunkList(got.file),
                    encode_stored(&list.to_envelope()),
                )
                .await
                .unwrap();
        }
    }
    Change::write(kind, got.file, got.size, got.git, None).unwrap()
}

/// A tree of `(path, kind, content)` leaves.
async fn tree_of(e: &TestEngine, files: &[(&str, Kind, &[u8])]) -> TreeId {
    let mut changes = Vec::new();
    for (path, kind, content) in files {
        changes.push((p(path), put_file(e, *kind, content).await));
    }
    e.apply(Engine::<BucketObjects, NoRefs>::empty_tree_id(), &changes)
        .await
        .unwrap()
}

async fn run<F>(e: &TestEngine, a: TreeId, b: TreeId, opts: &DiffOptions, f: F) -> String
where
    F: for<'a> FnOnce(
        &'a mut Vec<u8>,
        &'a DiffCtx<'a>,
        &'a [TreeChange],
    ) -> futures::future::BoxFuture<'a, Result<()>>,
{
    let changes = e.diff_trees(a, b, None, opts).await.unwrap();
    let ctx = DiffCtx {
        opts,
        content: e,
        attrs: &NoAttributes,
    };
    let mut out = Vec::new();
    f(&mut out, &ctx, &changes).await.unwrap();
    String::from_utf8(out).unwrap()
}

// ---- the committed scenario ----

fn scenario_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/diff_golden/scenario")
}

fn expected(name: &str) -> String {
    let bytes = std::fs::read(scenario_dir().join("expected").join(name)).unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, Vec<u8>)>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        let path = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        if entry.file_type().unwrap().is_dir() {
            walk(&entry.path(), &path, out);
        } else {
            out.push((path, std::fs::read(entry.path()).unwrap()));
        }
    }
}

async fn scenario_tree(e: &TestEngine, which: &str) -> TreeId {
    let modes = std::fs::read_to_string(scenario_dir().join("modes.txt")).unwrap();
    let mut files = Vec::new();
    walk(&scenario_dir().join(which), "", &mut files);
    let mut changes = Vec::new();
    for (path, content) in files {
        let exec = modes
            .lines()
            .any(|l| l == format!("{which} {path} 100755").as_str());
        let kind = if exec { Kind::Exec } else { Kind::File };
        changes.push((p(&path), put_file(e, kind, &content).await));
    }
    e.apply(TestEngine::empty_tree_id(), &changes)
        .await
        .unwrap()
}

async fn scenario() -> (TestEngine, TreeId, TreeId) {
    let e = engine();
    let (a, b) = (scenario_tree(&e, "c1").await, scenario_tree(&e, "c2").await);
    (e, a, b)
}

fn renames() -> DiffOptions {
    DiffOptions {
        renames: Some(RenameOptions::default()),
        ..DiffOptions::default()
    }
}

macro_rules! sync_out {
    ($f:path) => {
        |w, ctx, changes| Box::pin(async move { $f(w, changes, ctx.opts) })
    };
}

macro_rules! async_out {
    ($f:path) => {
        |w, ctx, changes| Box::pin($f(w, ctx, changes))
    };
}

#[tokio::test]
async fn patch_matches_git() {
    let (e, a, b) = scenario().await;
    let got = run(&e, a, b, &renames(), async_out!(write_patch)).await;
    assert_eq!(got, expected("patch"));
}

#[tokio::test]
async fn patch_without_renames_matches_git() {
    let (e, a, b) = scenario().await;
    let got = run(&e, a, b, &DiffOptions::default(), async_out!(write_patch)).await;
    assert_eq!(got, expected("patch-no-renames"));
}

#[tokio::test]
async fn context_and_algorithm_options_match_git() {
    let (e, a, b) = scenario().await;
    for (name, tweak) in [
        (
            "patch-u1",
            Box::new(|o: &mut DiffOptions| o.context = 1) as Box<dyn Fn(&mut DiffOptions)>,
        ),
        ("patch-u0", Box::new(|o| o.context = 0)),
        (
            "patch-histogram",
            Box::new(|o| o.algorithm = Algorithm::Histogram),
        ),
        (
            "patch-patience",
            Box::new(|o| o.algorithm = Algorithm::Patience),
        ),
        ("patch-noprefix", Box::new(|o| o.no_prefix = true)),
    ] {
        let mut opts = renames();
        tweak(&mut opts);
        let got = run(&e, a, b, &opts, async_out!(write_patch)).await;
        assert_eq!(got, expected(name), "{name}");
    }
}

#[tokio::test]
async fn relative_hides_and_strips() {
    let (e, a, b) = scenario().await;
    let opts = DiffOptions {
        relative: Some(p("docs")),
        ..renames()
    };
    assert_eq!(
        run(&e, a, b, &opts, async_out!(write_patch)).await,
        expected("patch-relative")
    );
    assert_eq!(
        run(&e, a, b, &opts, sync_out!(write_name_only)).await,
        expected("name-only-relative")
    );
}

#[tokio::test]
async fn listing_formats_match_git() {
    let (e, a, b) = scenario().await;
    let o = renames();
    assert_eq!(
        run(&e, a, b, &o, sync_out!(write_name_only)).await,
        expected("name-only")
    );
    assert_eq!(
        run(&e, a, b, &o, sync_out!(write_name_status)).await,
        expected("name-status")
    );
    assert_eq!(
        run(&e, a, b, &o, sync_out!(write_raw)).await,
        expected("raw")
    );
    assert_eq!(
        run(&e, a, b, &o, sync_out!(write_summary)).await,
        expected("summary")
    );
    let abbrev = DiffOptions {
        abbrev: 10,
        ..renames()
    };
    assert_eq!(
        run(&e, a, b, &abbrev, sync_out!(write_raw)).await,
        expected("raw-abbrev10")
    );
    let z = DiffOptions {
        nul: true,
        ..renames()
    };
    assert_eq!(
        run(&e, a, b, &z, sync_out!(write_name_status)).await,
        expected("name-status-z")
    );
    assert_eq!(
        run(&e, a, b, &z, async_out!(write_numstat)).await,
        expected("numstat-z")
    );
}

#[tokio::test]
async fn stat_formats_match_git() {
    let (e, a, b) = scenario().await;
    let o = renames();
    assert_eq!(
        run(&e, a, b, &o, async_out!(write_stat)).await,
        expected("stat")
    );
    assert_eq!(
        run(&e, a, b, &o, async_out!(write_numstat)).await,
        expected("numstat")
    );
    assert_eq!(
        run(&e, a, b, &o, async_out!(write_shortstat)).await,
        expected("shortstat")
    );
    for (name, width, name_width, count) in [
        ("stat60", Some(60), None, None),
        ("stat40", Some(40), None, None),
        ("stat-name20", Some(80), Some(20), None),
        ("stat-graph5", Some(80), Some(40), Some(5)),
    ] {
        let opts = DiffOptions {
            stat_width: width,
            stat_name_width: name_width,
            stat_count: count,
            ..renames()
        };
        let got = run(&e, a, b, &opts, async_out!(write_stat)).await;
        assert_eq!(got, expected(name), "{name}");
    }
}

#[tokio::test]
async fn word_diff_matches_git() {
    let (e, a, b) = scenario().await;
    let got = run(&e, a, b, &renames(), async_out!(write_word_diff)).await;
    assert_eq!(got, expected("word-diff"));
}

// ---- tree changes ----

#[tokio::test]
async fn tree_changes_report_status_and_both_entries() {
    let e = engine();
    let a = tree_of(
        &e,
        &[
            ("keep", Kind::File, b"same\n"),
            ("mod", Kind::File, b"one\n"),
            ("gone", Kind::File, b"x\n"),
            ("link", Kind::File, b"target"),
            ("dir/inner", Kind::File, b"i\n"),
        ],
    )
    .await;
    let b = tree_of(
        &e,
        &[
            ("keep", Kind::File, b"same\n"),
            ("mod", Kind::File, b"two\n"),
            ("new", Kind::File, b"n\n"),
            ("link", Kind::Symlink, b"target"),
            ("dir/inner", Kind::File, b"i\n"),
        ],
    )
    .await;
    let changes = e.tree_changes(a, b, None).await.unwrap();
    let got: Vec<_> = changes
        .iter()
        .map(|c| (c.path.to_string(), c.status))
        .collect();
    assert_eq!(
        got,
        vec![
            ("gone".to_string(), Status::Deleted),
            ("link".to_string(), Status::TypeChanged),
            ("mod".to_string(), Status::Modified),
            ("new".to_string(), Status::Added),
        ]
    );
    let m = &changes[2];
    assert_eq!(m.old.as_ref().unwrap().size, 4);
    assert_eq!(m.new.as_ref().unwrap().kind, Kind::File);
    assert!(changes[0].new.is_none() && changes[3].old.is_none());
}

#[tokio::test]
async fn type_change_is_a_deletion_then_a_creation_in_patches() {
    let e = engine();
    let a = tree_of(&e, &[("l", Kind::File, b"data\n")]).await;
    let b = tree_of(&e, &[("l", Kind::Symlink, b"elsewhere")]).await;
    let out = run(&e, a, b, &DiffOptions::default(), async_out!(write_patch)).await;
    assert_eq!(
        out,
        "diff --git a/l b/l\ndeleted file mode 100644\nindex 8d6a4d8..0000000\n--- a/l\n+++ /dev/null\n@@ -1 +0,0 @@\n-data\n\
         diff --git a/l b/l\nnew file mode 120000\nindex 0000000..eae7a99\n--- /dev/null\n+++ b/l\n@@ -0,0 +1 @@\n+elsewhere\n\\ No newline at end of file\n"
            .replace("8d6a4d8", &asc_core::git::blob_oid(b"data\n").to_string()[..7])
            .replace("eae7a99", &asc_core::git::blob_oid(b"elsewhere").to_string()[..7])
    );
}

// ---- renames ----

fn named(v: &[(String, Vec<u8>)]) -> Vec<(&str, Kind, &[u8])> {
    v.iter()
        .map(|(n, c)| (n.as_str(), Kind::File, c.as_slice()))
        .collect()
}

#[tokio::test]
async fn exact_rename_needs_no_content_read_and_scores_100() {
    let e = engine();
    let body = "some file body\n".repeat(20);
    let a = tree_of(&e, &[("old/name.txt", Kind::File, body.as_bytes())]).await;
    let b = tree_of(&e, &[("new/name.txt", Kind::File, body.as_bytes())]).await;
    let changes = e.diff_trees(a, b, None, &renames()).await.unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].status, Status::Renamed(100));
    assert_eq!(changes[0].old_path, p("old/name.txt"));
    assert_eq!(changes[0].path, p("new/name.txt"));
}

#[tokio::test]
async fn exact_rename_prefers_the_same_file_name() {
    let e = engine();
    let body = b"identical\n";
    let a = tree_of(
        &e,
        &[
            ("x/keep.txt", Kind::File, body),
            ("y/other.txt", Kind::File, body),
        ],
    )
    .await;
    let b = tree_of(&e, &[("z/keep.txt", Kind::File, body)]).await;
    let changes = e.diff_trees(a, b, None, &renames()).await.unwrap();
    let renamed = changes.iter().find(|c| c.status.score().is_some()).unwrap();
    assert_eq!(renamed.old_path, p("x/keep.txt"));
    assert!(
        changes
            .iter()
            .any(|c| c.path == p("y/other.txt") && c.status == Status::Deleted)
    );
}

#[tokio::test]
async fn similarity_below_threshold_is_not_a_rename() {
    let e = engine();
    let text = |tail: &str| -> String {
        (0..10)
            .map(|i| {
                if i < 3 {
                    format!("shared {i}\n")
                } else {
                    format!("{tail} line {i}\n")
                }
            })
            .collect()
    };
    let (old, new) = (text("old"), text("new"));
    let a = tree_of(&e, &[("a", Kind::File, old.as_bytes())]).await;
    let b = tree_of(&e, &[("b", Kind::File, new.as_bytes())]).await;
    let changes = e.diff_trees(a, b, None, &renames()).await.unwrap();
    assert_eq!(
        changes.iter().map(|c| c.status).collect::<Vec<_>>(),
        vec![Status::Deleted, Status::Added]
    );
    let lenient = DiffOptions {
        renames: Some(RenameOptions {
            threshold: 10,
            ..RenameOptions::default()
        }),
        ..DiffOptions::default()
    };
    let changes = e.diff_trees(a, b, None, &lenient).await.unwrap();
    assert_eq!(changes.len(), 1);
    assert!(matches!(changes[0].status, Status::Renamed(s) if (10..50).contains(&s)));
}

#[tokio::test]
async fn rename_limit_disables_inexact_matching_only() {
    let e = engine();
    let text = |tag: &str| -> String {
        (0..20)
            .map(|i| format!("line {i} {}\n", if i == 0 { tag } else { "same" }))
            .collect()
    };
    let a = tree_of(
        &e,
        &[
            ("a1", Kind::File, text("a1").as_bytes()),
            ("a2", Kind::File, b"exact\n"),
        ],
    )
    .await;
    let b = tree_of(
        &e,
        &[
            ("b1", Kind::File, text("b1").as_bytes()),
            ("b2", Kind::File, b"exact\n"),
        ],
    )
    .await;
    let capped = DiffOptions {
        renames: Some(RenameOptions {
            limit: 1,
            ..RenameOptions::default()
        }),
        ..DiffOptions::default()
    };
    let changes = e.diff_trees(a, b, None, &capped).await.unwrap();
    // The exact pair is found; the near pair would need both sets to fit the limit... it does
    // (one each), so it is found too. Two candidates each side exceed the limit.
    assert_eq!(changes.len(), 2);
    let many_a: Vec<(String, Vec<u8>)> = (0..3)
        .map(|i| (format!("o{i}"), text(&format!("o{i}")).into_bytes()))
        .collect();
    let many_b: Vec<(String, Vec<u8>)> = (0..3)
        .map(|i| (format!("n{i}"), text(&format!("n{i}")).into_bytes()))
        .collect();
    let a = tree_of(&e, &named(&many_a)).await;
    let b = tree_of(&e, &named(&many_b)).await;
    let capped = DiffOptions {
        renames: Some(RenameOptions {
            limit: 2,
            ..RenameOptions::default()
        }),
        ..DiffOptions::default()
    };
    assert_eq!(e.diff_trees(a, b, None, &capped).await.unwrap().len(), 6);
    assert_eq!(e.diff_trees(a, b, None, &renames()).await.unwrap().len(), 3);
}

// ---- attributes ----

struct Attrs;

#[async_trait]
impl Attributes for Attrs {
    fn diff_attr(&self, path: &RepoPath) -> DiffAttr {
        match path.as_bytes() {
            b"data.bin" => DiffAttr::Binary,
            b"text.dat" => DiffAttr::Text,
            b"conv.pdf" => DiffAttr::Driver("pdf".into()),
            _ => DiffAttr::Unspecified,
        }
    }

    async fn textconv(&self, driver: &str, _: &RepoPath, content: &Bytes) -> Result<Option<Bytes>> {
        assert_eq!(driver, "pdf");
        // A stand-in converter: upper-cases the bytes.
        Ok(Some(Bytes::from(content.to_ascii_uppercase())))
    }
}

#[tokio::test]
async fn attributes_override_binary_detection_and_convert_text() {
    let e = engine();
    let a = tree_of(
        &e,
        &[
            ("data.bin", Kind::File, b"plain\n"),
            ("text.dat", Kind::File, b"a\0b\n"),
            ("conv.pdf", Kind::File, b"abc\0def\n"),
        ],
    )
    .await;
    let b = tree_of(
        &e,
        &[
            ("data.bin", Kind::File, b"plain2\n"),
            ("text.dat", Kind::File, b"a\0c\n"),
            ("conv.pdf", Kind::File, b"abc\0xyz\n"),
        ],
    )
    .await;
    let opts = DiffOptions::default();
    let changes = e.tree_changes(a, b, None).await.unwrap();
    let ctx = DiffCtx {
        opts: &opts,
        content: &e,
        attrs: &Attrs,
    };
    let mut out = Vec::new();
    write_patch(&mut out, &ctx, &changes).await.unwrap();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("Binary files a/data.bin and b/data.bin differ"));
    assert!(!out.contains("Binary files a/text.dat"));
    assert!(out.contains("-a\0b\n+a\0c\n"));
    // textconv output is what gets diffed.
    assert!(out.contains("-ABC\0DEF\n+ABC\0XYZ\n"));
    let mut numstat = Vec::new();
    write_numstat(&mut numstat, &ctx, &changes).await.unwrap();
    assert_eq!(
        String::from_utf8(numstat).unwrap(),
        "1\t1\tconv.pdf\n-\t-\tdata.bin\n1\t1\ttext.dat\n"
    );
}

#[tokio::test]
async fn files_over_the_big_file_threshold_are_binary_without_a_read() {
    struct Refuses;
    #[async_trait]
    impl ContentSource for Refuses {
        async fn read(&self, _: &asc_core::object::Entry) -> Result<Bytes> {
            panic!("content must not be read");
        }
    }
    let e = engine();
    let a = tree_of(&e, &[("big", Kind::File, b"0123456789\n")]).await;
    let b = tree_of(&e, &[("big", Kind::File, b"0123456780\n")]).await;
    let opts = DiffOptions {
        big_file_threshold: 5,
        ..DiffOptions::default()
    };
    let changes = e.tree_changes(a, b, None).await.unwrap();
    let ctx = DiffCtx {
        opts: &opts,
        content: &Refuses,
        attrs: &NoAttributes,
    };
    let mut out = Vec::new();
    write_patch(&mut out, &ctx, &changes).await.unwrap();
    assert!(
        String::from_utf8(out)
            .unwrap()
            .ends_with("Binary files a/big and b/big differ\n")
    );
}

#[tokio::test]
async fn git_style_paths_are_quoted_and_unquoted_with_z() {
    let e = engine();
    let a = tree_of(&e, &[]).await;
    let b = tree_of(&e, &[("caf\u{e9}\ttab", Kind::File, b"x\n")]).await;
    let out = run(
        &e,
        a,
        b,
        &DiffOptions::default(),
        sync_out!(write_name_only),
    )
    .await;
    assert_eq!(out, "\"caf\\303\\251\\ttab\"\n");
    let z = DiffOptions {
        nul: true,
        ..DiffOptions::default()
    };
    let out = run(&e, a, b, &z, sync_out!(write_name_only)).await;
    assert_eq!(out, "caf\u{e9}\ttab\0");
}

#[tokio::test]
async fn chunked_files_are_read_back_whole() {
    let e = engine();
    let big: Vec<u8> = (0..5_000_000u32)
        .flat_map(|i| (i % 251).to_le_bytes())
        .take(5_000_000)
        .collect();
    let change = put_file(&e, Kind::File, &big).await;
    let Change::Write { file, size, .. } = change else {
        panic!()
    };
    assert!(size > asc_core::content::CHUNK_THRESHOLD);
    assert_eq!(
        e.read_file(file, size).await.unwrap().as_ref(),
        big.as_slice()
    );
}

#[tokio::test]
async fn a_forged_huge_size_is_refused_before_any_read() {
    let e = engine();
    let file = asc_core::id::FileId::of(b"claimed");
    // Nothing is stored for `file`: the bound alone decides, so no buffer is reserved and
    // no object is fetched for a size past the limit.
    let err = e
        .read_file(file, MAX_READ_FILE_BYTES + 1)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::FileTooLarge(f, _) if f == file),
        "{err}"
    );
    let err = e.read_file(file, u64::MAX).await.unwrap_err();
    assert!(matches!(err, Error::FileTooLarge(..)), "{err}");
    // At the bound the read proceeds (and finds nothing stored).
    let err = e.read_file(file, MAX_READ_FILE_BYTES).await.unwrap_err();
    assert!(matches!(err, Error::MissingFile(_)), "{err}");
}

#[tokio::test]
async fn forged_chunk_list_is_rejected() {
    let e = engine();
    let pattern = |m: u32| -> Vec<u8> {
        (0..5_000_000u32)
            .flat_map(|i| (i.wrapping_mul(m) % 251).to_le_bytes())
            .take(5_000_000)
            .collect()
    };
    let honest = put_file(&e, Kind::File, &pattern(1)).await;
    let evil = put_file(&e, Kind::File, &pattern(7)).await;
    let (
        Change::Write { file, size, .. },
        Change::Write {
            file: evil_file, ..
        },
    ) = (honest, evil)
    else {
        panic!()
    };
    // A remote swaps in a valid list of other chunks of the same length.
    let forged = e
        .objects
        .get_stored(&ObjKey::ChunkList(evil_file))
        .await
        .unwrap()
        .unwrap();
    e.objects.delete(&ObjKey::ChunkList(file)).await.unwrap();
    e.objects
        .put(&ObjKey::ChunkList(file), forged)
        .await
        .unwrap();
    assert!(matches!(
        e.read_file(file, size).await,
        Err(Error::MissingFile(f)) if f == file
    ));
    // A list whose size disagrees with the entry is refused too.
    assert!(e.read_file(file, size - 1).await.is_err());
}
