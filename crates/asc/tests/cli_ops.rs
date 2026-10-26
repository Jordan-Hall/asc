//! U40b acceptance: the operational verbs through the built binary: `import`, `export`,
//! `prefetch`, `debug`, `fsck`, `sparse-checkout`/`sparse`, `archive`, the `repack`/`prune`/
//! `maintenance` no-ops and `migrate ignore`.

mod support;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use asc_store::refs::Refs as _;
use support::{Fixture, Sandbox, text};

/// `asc args…` in the fixture's workspace, without a daemon.
fn asc(f: &Fixture, args: &[&str]) -> Output {
    f.asc()
        .env("ASC_NO_DAEMON", "1")
        .args(args)
        .output()
        .expect("spawn asc")
}

fn out(o: &Output) -> String {
    text(&o.stdout)
}

fn err(o: &Output) -> String {
    text(&o.stderr)
}

fn ok(o: &Output) -> String {
    assert!(o.status.success(), "stdout: {}\nstderr: {}", out(o), err(o));
    out(o)
}

/// Real git in `dir`, hermetic; returns trimmed stdout.
fn git(dir: &Path, sb: &Sandbox, args: &[&str]) -> String {
    let o = Command::new("git") // lint:allow-raw-git
        .current_dir(dir)
        .env("HOME", &sb.user)
        .env("USERPROFILE", &sb.user)
        .env("GIT_CONFIG_GLOBAL", sb.user.join(".gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "2024-01-02T03:04:05Z")
        .env("GIT_COMMITTER_DATE", "2024-01-02T03:04:05Z")
        .args(args)
        .output()
        .expect("spawn git");
    assert!(o.status.success(), "git {args:?}: {}", err(&o));
    out(&o).trim().to_owned()
}

/// A git repository with two commits on `master` and an annotated tag `v1` on the first.
/// The repository, its first and second commit and the annotated tag object.
type Source = (std::path::PathBuf, String, String, String);

fn git_source(sb: &Sandbox) -> Source {
    let dir = sb.dir("src.git-work");
    git(&dir, sb, &["init", "-q", "-b", "master"]);
    fs::write(dir.join("a.txt"), "one\n").unwrap();
    fs::create_dir_all(dir.join("sub")).unwrap();
    fs::write(dir.join("sub").join("b.txt"), "bee\n").unwrap();
    git(&dir, sb, &["add", "-A"]);
    git(&dir, sb, &["commit", "-q", "-m", "first"]);
    let first = git(&dir, sb, &["rev-parse", "HEAD"]);
    git(&dir, sb, &["tag", "-a", "-m", "release one", "v1"]);
    let tag = git(&dir, sb, &["rev-parse", "v1"]);
    fs::write(dir.join("a.txt"), "two\n").unwrap();
    git(&dir, sb, &["commit", "-q", "-am", "second"]);
    let second = git(&dir, sb, &["rev-parse", "HEAD"]);
    (dir, first, second, tag)
}

/// A fixture workspace whose HEAD is `master` at a commit with these files, written to disk too.
async fn workspace(files: &[(&str, &str)]) -> (Fixture, asc_core::id::ChangesetId) {
    let f = Fixture::new().await;
    let pairs: Vec<(&str, &[u8])> = files.iter().map(|(p, c)| (*p, c.as_bytes())).collect();
    let cs = f.commit(&[], &pairs).await;
    f.set_branch("master", cs).await;
    f.checkout_branch("master", Some(cs)).await;
    for (p, c) in files {
        let path = f.root.join(p);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, c).unwrap();
    }
    (f, cs)
}

// ------------------------------------------------------------------ import and export

#[tokio::test]
async fn import_then_export_gives_back_the_same_shas() {
    let f = Fixture::new().await;
    let (src, first, second, tag) = git_source(&f.sandbox);

    let imported = ok(&asc(&f, &["import", "--git", src.to_str().unwrap()]));
    assert!(imported.contains("1 branches and 1 tags"), "{imported}");

    // Branches land under refs/remotes/import, tags keep their names.
    let names: Vec<String> =
        f.db.refs()
            .list(f.repo, "refs/")
            .await
            .unwrap()
            .iter()
            .map(|e| String::from_utf8_lossy(e.name.as_bytes()).into_owned())
            .collect();
    assert!(
        names.contains(&"refs/remotes/import/master".to_owned()),
        "{names:?}"
    );
    assert!(names.contains(&"refs/tags/v1".to_owned()), "{names:?}");

    // A second import has nothing new to do.
    let again = ok(&asc(
        &f,
        &["import", "--git", src.to_str().unwrap(), "--incremental"],
    ));
    assert!(again.contains("0 refs updated"), "{again}");

    // Export back out: the git SHAs are identical, and real git accepts the result.
    let dst = f.sandbox.tmp.path().join("exported.git");
    let exported = ok(&asc(
        &f,
        &[
            "export",
            "--git",
            dst.to_str().unwrap(),
            "--refs",
            "refs/remotes/import/*",
            "--refs",
            "refs/tags/*",
        ],
    ));
    assert!(exported.contains("2 refs"), "{exported}");
    assert_eq!(
        git(
            &dst,
            &f.sandbox,
            &["rev-parse", "refs/remotes/import/master"]
        ),
        second
    );
    assert_eq!(git(&dst, &f.sandbox, &["rev-parse", "refs/tags/v1"]), tag);
    assert_eq!(
        git(&dst, &f.sandbox, &["rev-parse", "refs/tags/v1^{commit}"]),
        first
    );
    git(&dst, &f.sandbox, &["fsck", "--strict"]);

    // Exporting again writes nothing new.
    let twice = ok(&asc(
        &f,
        &[
            "export",
            "--git",
            dst.to_str().unwrap(),
            "--refs=refs/remotes/import/*",
        ],
    ));
    assert!(twice.contains("exported 0 objects"), "{twice}");
}

#[tokio::test]
async fn import_names_and_urls_are_checked() {
    let f = Fixture::new().await;
    let o = asc(&f, &["import"]);
    assert_eq!(o.status.code(), Some(129), "{}", err(&o));
    let o = asc(&f, &["import", "--git", "x", "--repo", "../evil"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("not a usable import name"), "{}", err(&o));
    // A remote-helper URL is refused before git is run.
    let o = asc(&f, &["import", "--git", "ext::sh -c id"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("helper"), "{}", err(&o));
    let o = asc(&f, &["import", "--git", "-oProxyCommand=x"]);
    assert!(!o.status.success());
}

#[tokio::test]
async fn export_refuses_a_directory_that_is_not_a_repository() {
    let (f, _) = workspace(&[("a.txt", "a\n")]).await;
    let dir = f.sandbox.dir("occupied");
    fs::write(dir.join("keep.txt"), "mine").unwrap();
    let o = asc(&f, &["export", "--git", dir.to_str().unwrap()]);
    assert!(!o.status.success());
    assert!(err(&o).contains("not empty"), "{}", err(&o));
    assert_eq!(fs::read(dir.join("keep.txt")).unwrap(), b"mine");
}

#[tokio::test]
async fn export_into_a_fresh_directory_creates_a_bare_repository() {
    let (f, cs) = workspace(&[("a.txt", "a\n"), ("d/b.txt", "b\n")]).await;
    let dst = f.sandbox.tmp.path().join("fresh");
    ok(&asc(&f, &["export", "--git", dst.to_str().unwrap()]));
    let sha = git(&dst, &f.sandbox, &["rev-parse", "refs/heads/master"]);
    assert_eq!(sha.len(), 40);
    assert_eq!(git(&dst, &f.sandbox, &["show", "master:d/b.txt"]), "b");
    git(&dst, &f.sandbox, &["fsck", "--strict"]);
    let _ = cs;
}

// ------------------------------------------------------------------ prefetch

#[tokio::test]
async fn prefetch_reads_files_and_history() {
    let (f, cs) = workspace(&[
        ("src/a.txt", "a\n"),
        ("src/b.txt", "b\n"),
        ("doc/c.txt", "c\n"),
    ])
    .await;
    let second = f.commit(&[cs], &[("src/a.txt", b"a2\n")]).await;
    f.set_branch("master", second).await;
    f.checkout_branch("master", Some(second)).await;

    let o = ok(&asc(&f, &["prefetch", "src", "--history", "2"]));
    assert!(o.contains("prefetched 2 files"), "{o}");
    assert!(o.contains("and 2 commits"), "{o}");

    let o = ok(&asc(&f, &["prefetch", "."]));
    assert!(o.contains("prefetched 3 files"), "{o}");

    // A missing object is reported and fails the command.
    let leaf = f.file_leaf(second, "doc/c.txt").await;
    let key = asc_store::key::ObjKey::File(leaf.file);
    asc_store::objects::Objects::delete(&*f.objects, &key)
        .await
        .unwrap();
    let o = asc(&f, &["prefetch", "doc"]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    assert!(err(&o).contains("doc/c.txt"), "{}", err(&o));
}

#[tokio::test]
async fn prefetch_usage() {
    let (f, _) = workspace(&[("a.txt", "a\n")]).await;
    assert_eq!(asc(&f, &["prefetch"]).status.code(), Some(129));
    assert_eq!(
        asc(&f, &["prefetch", "--history", "many", "a.txt"])
            .status
            .code(),
        Some(129)
    );
}

// ------------------------------------------------------------------ debug

#[tokio::test]
async fn debug_stats_and_hydrations_without_a_daemon() {
    let (f, _) = workspace(&[("a.txt", "hello\n")]).await;
    let o = ok(&asc(&f, &["debug", "stats", "--json"]));
    let v: serde_json::Value = serde_json::from_str(o.trim()).unwrap();
    assert!(v["cache"]["bytes"].is_u64(), "{v}");
    assert!(v["store"]["files"].as_u64().unwrap() >= 1, "{v}");
    assert!(v["daemon"].is_null(), "{v}");
    let ws = &v["workspaces"][0];
    assert_eq!(ws["mode"], "plain", "{v}");
    assert!(ws["bytes"].as_u64().unwrap() >= 6, "{v}");

    let text_form = ok(&asc(&f, &["debug", "stats"]));
    assert!(text_form.contains("cache: "), "{text_form}");
    assert!(text_form.contains("daemon: not running"), "{text_form}");

    let h = ok(&asc(&f, &["debug", "hydrations", "--by-process"]));
    assert!(h.contains("daemon is not running"), "{h}");
    assert_eq!(
        ok(&asc(&f, &["debug", "hydrations", "--json"])).trim(),
        "[]"
    );

    let t = asc(&f, &["debug", "trace"]);
    assert!(!t.status.success());
    assert!(err(&t).contains("daemon is not running"), "{}", err(&t));

    assert_eq!(asc(&f, &["debug"]).status.code(), Some(129));
    assert_eq!(asc(&f, &["debug", "bogus"]).status.code(), Some(129));
}

// ------------------------------------------------------------------ fsck

#[tokio::test]
async fn fsck_is_silent_when_clean_and_writes_generations() {
    let (f, cs) = workspace(&[("a.txt", "a\n")]).await;
    let second = f.commit(&[cs], &[("b.txt", b"b\n")]).await;
    f.set_branch("master", second).await;
    f.checkout_branch("master", Some(second)).await;

    let o = asc(&f, &["fsck"]);
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(out(&o), "");
    // The fixture wrote no cs_gen rows; the first fsck derived both, the second finds them.
    let rows = asc_state::state::tables::cs_gen::get(&f.db, f.repo, second)
        .await
        .unwrap();
    assert_eq!(rows.unwrap().gen_no, 2);
    let first = asc_state::state::tables::cs_gen::get(&f.db, f.repo, cs)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((first.gen_no, first.parents.len()), (1, 0));
    let v = ok(&asc(&f, &["fsck", "-v", "--full"]));
    assert!(v.contains("0 generation rows written"), "{v}");
    assert!(v.contains("objects in store/"), "{v}");
}

#[tokio::test]
async fn fsck_reports_a_corrupt_object_and_leaves_it() {
    let (f, _) = workspace(&[("a.txt", "a\n")]).await;
    // Overwrite the one file object's bytes under store/.
    let leaf_dir = f.home.store().join("f");
    let victim = walk_files(&leaf_dir).pop().expect("a file object");
    fs::write(&victim, b"\0not what the key says").unwrap();
    let o = asc(&f, &["fsck"]);
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
    assert!(err(&o).contains("error: store/f/"), "{}", err(&o));
    assert!(victim.exists(), "fsck must not delete what it reports");
}

fn walk_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                found.push(p);
            }
        }
    }
    found
}

#[tokio::test]
async fn fsck_reports_a_wrong_generation_and_a_wrong_parent_list() {
    let (f, cs) = workspace(&[(
        "a.txt", "a
",
    )])
    .await;
    assert!(asc(&f, &["fsck"]).status.success());
    let set = |column: &'static str| {
        let q = format!("UPDATE cs_gen SET {column} = ? WHERE repo_id = ? AND cs = ?");
        (q, f.repo.0, cs.as_bytes().to_vec())
    };

    // A root has generation 1.
    let (q, repo, id) = set("gen");
    sqlx::query(sqlx::AssertSqlSafe(q))
        .bind(41i64)
        .bind(repo)
        .bind(id)
        .execute(f.db.pool())
        .await
        .unwrap();
    let o = asc(&f, &["fsck"]);
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
    assert!(
        err(&o).contains("generation 41 but its parents give 1"),
        "{}",
        err(&o)
    );

    // Put it right; now the stored parent list is wrong.
    let (q, repo, id) = set("gen");
    sqlx::query(sqlx::AssertSqlSafe(q))
        .bind(1i64)
        .bind(repo)
        .bind(id)
        .execute(f.db.pool())
        .await
        .unwrap();
    assert!(asc(&f, &["fsck"]).status.success());
    let (q, repo, id) = set("parents");
    sqlx::query(sqlx::AssertSqlSafe(q))
        .bind(vec![9u8; 32])
        .bind(repo)
        .bind(id)
        .execute(f.db.pool())
        .await
        .unwrap();
    let o = asc(&f, &["fsck"]);
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
    assert!(err(&o).contains("parents differ"), "{}", err(&o));
}

#[tokio::test]
async fn fsck_usage() {
    let (f, _) = workspace(&[("a.txt", "a\n")]).await;
    assert_eq!(asc(&f, &["fsck", "HEAD"]).status.code(), Some(129));
    assert_eq!(asc(&f, &["fsck", "--lost-found"]).status.code(), Some(129));
}

// ------------------------------------------------------------------ sparse

#[tokio::test]
async fn sparse_checkout_trims_and_restores_the_work_tree() {
    let (f, _) = workspace(&[
        ("top.txt", "top\n"),
        ("a/x.txt", "ax\n"),
        ("b/y.txt", "by\n"),
        ("c/z.txt", "cz\n"),
    ])
    .await;
    let root = &f.root;

    let o = asc(&f, &["sparse-checkout", "list"]);
    assert_eq!(o.status.code(), Some(128), "{}", err(&o));
    assert!(err(&o).contains("not sparse"), "{}", err(&o));

    ok(&asc(&f, &["sparse-checkout", "set", "--cone", "a"]));
    assert_eq!(ok(&asc(&f, &["sparse-checkout", "list"])).trim(), "a");
    assert!(root.join("top.txt").exists() && root.join("a/x.txt").exists());
    assert!(!root.join("b/y.txt").exists() && !root.join("c/z.txt").exists());
    assert!(!root.join("b").exists(), "emptied directories go");

    ok(&asc(&f, &["sparse", "add", "b"]));
    assert_eq!(
        ok(&asc(&f, &["sparse", "list"])).replace("\r\n", "\n"),
        "a\nb\n"
    );
    assert_eq!(fs::read(root.join("b/y.txt")).unwrap(), b"by\n");
    assert!(!root.join("c").exists());

    // A modified file that leaves the cone is kept, with a warning.
    fs::write(root.join("b/y.txt"), "edited\n").unwrap();
    let o = asc(&f, &["sparse-checkout", "set", "a"]);
    assert!(o.status.success(), "{}", err(&o));
    assert!(err(&o).contains("b/y.txt"), "{}", err(&o));
    assert_eq!(fs::read(root.join("b/y.txt")).unwrap(), b"edited\n");

    // An untracked file in the way of a path entering the cone is never overwritten.
    fs::create_dir_all(root.join("c")).unwrap();
    fs::write(root.join("c/z.txt"), "mine\n").unwrap();
    let o = asc(&f, &["sparse", "add", "c"]);
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(fs::read(root.join("c/z.txt")).unwrap(), b"mine\n");

    ok(&asc(&f, &["sparse-checkout", "disable"]));
    assert_eq!(
        asc(&f, &["sparse-checkout", "list"]).status.code(),
        Some(128)
    );
    assert_eq!(fs::read(root.join("a/x.txt")).unwrap(), b"ax\n");
}

#[tokio::test]
async fn sparse_rejects_bad_input() {
    let (f, _) = workspace(&[("a/x.txt", "ax\n")]).await;
    assert_eq!(
        asc(&f, &["sparse", "set", "../out"]).status.code(),
        Some(128)
    );
    assert_eq!(asc(&f, &["sparse", "init"]).status.code(), Some(129));
    assert_eq!(asc(&f, &["sparse", "list", "a"]).status.code(), Some(129));
    // Non-cone patterns are not served.
    assert_eq!(
        asc(&f, &["sparse-checkout", "set", "--no-cone", "a"])
            .status
            .code(),
        Some(129)
    );
    // A plain workspace has no placeholders to dehydrate.
    let o = asc(&f, &["sparse", "dehydrate", "a"]);
    assert_eq!(o.status.code(), Some(128), "{}", err(&o));
    assert!(err(&o).contains("virtual"), "{}", err(&o));
}

// ------------------------------------------------------------------ archive

/// Entries `(name, typeflag, data)` and the commit id of the global pax header.
type Untarred = (Vec<(String, u8, Vec<u8>)>, Option<String>);

/// A minimal tar reader: `(name, typeflag, data)` per entry, pax `path` records applied. The
/// header checksum of every block is verified.
fn untar(bytes: &[u8]) -> Untarred {
    let (mut pos, mut entries, mut global) = (0usize, Vec::new(), None);
    let mut pending: Option<String> = None;
    while pos + 512 <= bytes.len() {
        let h = &bytes[pos..pos + 512];
        if h.iter().all(|&b| b == 0) {
            break;
        }
        let field = |a: usize, b: usize| {
            String::from_utf8_lossy(&h[a..b])
                .trim_matches(|c| c == '\0' || c == ' ')
                .to_owned()
        };
        let stored = u32::from_str_radix(&field(148, 156), 8).expect("checksum field");
        let mut copy = h.to_vec();
        copy[148..156].fill(b' ');
        assert_eq!(
            stored,
            copy.iter().map(|&b| u32::from(b)).sum::<u32>(),
            "bad checksum"
        );
        let size = usize::from_str_radix(&field(124, 136), 8).unwrap();
        let flag = h[156];
        let data = bytes[pos + 512..pos + 512 + size].to_vec();
        pos += 512 + size.div_ceil(512) * 512;
        let records = |data: &[u8]| -> Vec<(String, String)> {
            let text = String::from_utf8_lossy(data).into_owned();
            text.lines()
                .filter_map(|l| l.split_once(' ').and_then(|(_, kv)| kv.split_once('=')))
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect()
        };
        match flag {
            b'x' => {
                pending = records(&data)
                    .into_iter()
                    .find(|(k, _)| k == "path")
                    .map(|(_, v)| v);
            }
            b'g' => {
                global = records(&data)
                    .into_iter()
                    .find(|(k, _)| k == "comment")
                    .map(|(_, v)| v);
            }
            _ => {
                let name = pending.take().unwrap_or_else(|| field(0, 100));
                let data = if flag == b'2' {
                    field(157, 257).into_bytes()
                } else {
                    data
                };
                entries.push((name, flag, data));
            }
        }
    }
    (entries, global)
}

const ATTRS: &str = "secret.txt export-ignore\nprivate export-ignore\nversion.txt export-subst\n";

async fn archive_fixture() -> (Fixture, String) {
    let long_dir = "d".repeat(60);
    let long = format!("{long_dir}/{long_dir}/file-with-a-long-name.txt");
    let (f, _) = workspace(&[
        (".gitattributes", ATTRS),
        ("readme.md", "hi\n"),
        ("secret.txt", "no\n"),
        ("private/inner.txt", "no\n"),
        ("src/main.rs", "fn main() {}\n"),
        ("version.txt", "id: $Format:%h|%s|%an$\n"),
        (&long, "long\n"),
    ])
    .await;
    (f, long)
}

#[tokio::test]
async fn archive_tar_honours_export_attributes() {
    let (f, long) = archive_fixture().await;
    let dest = f.sandbox.tmp.path().join("out.tar");
    ok(&asc(
        &f,
        &[
            "archive",
            "--prefix=pre/",
            "-o",
            dest.to_str().unwrap(),
            "HEAD",
        ],
    ));
    let (entries, commit) = untar(&fs::read(&dest).unwrap());
    let names: Vec<&str> = entries.iter().map(|(n, _, _)| n.as_str()).collect();
    assert!(names.contains(&"pre/"), "{names:?}");
    assert!(names.contains(&"pre/readme.md"), "{names:?}");
    assert!(names.contains(&"pre/src/"), "{names:?}");
    assert!(names.contains(&"pre/src/main.rs"), "{names:?}");
    assert!(
        !names
            .iter()
            .any(|n| n.contains("secret") || n.contains("private")),
        "{names:?}"
    );
    let long_name = format!("pre/{long}");
    assert!(names.contains(&long_name.as_str()), "pax path: {names:?}");
    let get = |n: &str| entries.iter().find(|(m, _, _)| m == n).unwrap().2.clone();
    assert_eq!(get("pre/src/main.rs"), b"fn main() {}\n");
    let version = String::from_utf8(get("pre/version.txt")).unwrap();
    assert!(
        version.starts_with("id: ") && version.contains("|commit of 7 files|Test"),
        "{version}"
    );
    assert_eq!(
        version.trim_end().len(),
        "id: 0000000|commit of 7 files|Test".len(),
        "{version}"
    );
    assert_eq!(commit.map(|c| c.len()), Some(40));
    // Dated with the commit time, not the clock.
    let bytes = fs::read(&dest).unwrap();
    let mtime = String::from_utf8_lossy(&bytes[136..148])
        .trim_matches(|c| c == ' ' || c == ' ')
        .to_owned();
    assert_eq!(u64::from_str_radix(&mtime, 8).unwrap(), 1_700_000_000);
}

#[tokio::test]
async fn archive_zip_and_path_selection() {
    let (f, _) = archive_fixture().await;
    let dest = f.sandbox.tmp.path().join("out.zip");
    // The format follows the file name; only src/ is selected.
    ok(&asc(
        &f,
        &["archive", "-o", dest.to_str().unwrap(), "HEAD", "src"],
    ));
    let mut zip = zip::ZipArchive::new(fs::File::open(&dest).unwrap()).unwrap();
    let mut names: Vec<String> = (0..zip.len())
        .map(|i| zip.by_index(i).unwrap().name().unwrap().to_string())
        .collect();
    names.sort();
    assert_eq!(names, vec!["src/", "src/main.rs"]);
    let mut body = String::new();
    std::io::Read::read_to_string(&mut zip.by_name("src/main.rs").unwrap(), &mut body).unwrap();
    assert_eq!(body, "fn main() {}\n");
}

#[tokio::test]
async fn archive_arguments() {
    let (f, _) = archive_fixture().await;
    assert_eq!(
        ok(&asc(&f, &["archive", "--list"])).replace("\r\n", "\n"),
        "tar\nzip\n"
    );
    assert_eq!(asc(&f, &["archive"]).status.code(), Some(129));
    let o = asc(&f, &["archive", "--format=rar", "HEAD"]);
    assert_eq!(o.status.code(), Some(128), "{}", err(&o));
    let o = asc(&f, &["archive", "-o", "x.tar.gz", "HEAD"]);
    assert!(err(&o).contains("compressed"), "{}", err(&o));
    let o = asc(&f, &["archive", "--prefix=../x/", "HEAD"]);
    assert_eq!(o.status.code(), Some(128));
    let o = asc(&f, &["archive", "nope-such-ref"]);
    assert_eq!(o.status.code(), Some(128));
    assert!(err(&o).contains("not a valid object name"), "{}", err(&o));
    // Standard output is a pipe here, so the archive streams there.
    let o = ok_bytes(&asc(&f, &["archive", "HEAD", "readme.md"]));
    let (entries, _) = untar(&o);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].2, b"hi\n");
}

fn ok_bytes(o: &Output) -> Vec<u8> {
    assert!(o.status.success(), "{}", err(o));
    o.stdout.clone()
}

// ------------------------------------------------------------------ no-ops and migrate

#[tokio::test]
async fn repack_prune_and_maintenance_are_no_ops() {
    let (f, _) = workspace(&[("a.txt", "a\n")]).await;
    for args in [
        &["repack", "-a", "-d"][..],
        &["prune", "--expire", "now"][..],
        &["maintenance", "run", "--auto"][..],
    ] {
        let o = asc(&f, args);
        assert!(o.status.success(), "{args:?}: {}", err(&o));
        assert!(
            err(&o).contains("not needed in asc"),
            "{args:?}: {}",
            err(&o)
        );
    }
    let o = asc(&f, &["repack", "-q"]);
    assert!(o.status.success() && err(&o).is_empty() && out(&o).is_empty());
    // A flag nobody serves is refused up front, like any command's.
    assert_eq!(asc(&f, &["prune", "--bogus"]).status.code(), Some(129));
}

#[tokio::test]
async fn migrate_ignore_copies_then_moves() {
    let (f, _) = workspace(&[("a.txt", "a\n")]).await;
    let root = &f.root;
    fs::write(root.join(".gitignore"), "target/\n").unwrap();
    fs::create_dir_all(root.join("pkg/inner")).unwrap();
    fs::write(root.join("pkg/.gitignore"), "dist/\n").unwrap();
    fs::write(root.join("pkg/inner/.gitignore"), "tmp\n").unwrap();
    fs::write(root.join("pkg/inner/.ascignore"), "different\n").unwrap();

    // Copy is the default; the original stays; a different .ascignore is never overwritten.
    let o = asc(&f, &["migrate", "ignore"]);
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
    assert!(
        out(&o).contains("copied .gitignore to .ascignore"),
        "{}",
        out(&o)
    );
    assert!(err(&o).contains("pkg/inner/.gitignore"), "{}", err(&o));
    assert_eq!(fs::read(root.join(".ascignore")).unwrap(), b"target/\n");
    assert_eq!(fs::read(root.join("pkg/.ascignore")).unwrap(), b"dist/\n");
    assert!(root.join(".gitignore").exists());
    assert_eq!(
        fs::read(root.join("pkg/inner/.ascignore")).unwrap(),
        b"different\n"
    );

    // --move renames; identical content just drops the original.
    fs::remove_file(root.join("pkg/inner/.gitignore")).unwrap();
    ok(&asc(&f, &["migrate", "ignore", "--move"]));
    assert!(!root.join(".gitignore").exists() && !root.join("pkg/.gitignore").exists());
    assert_eq!(fs::read(root.join(".ascignore")).unwrap(), b"target/\n");

    assert_eq!(asc(&f, &["migrate"]).status.code(), Some(129));
    assert_eq!(
        asc(&f, &["migrate", "ignore", "--bogus"]).status.code(),
        Some(129)
    );
    // `migrate branchless` is its own verb now (B19; tests/branchless_migrate.rs covers it).
    let b = asc(&f, &["migrate", "branchless"]);
    ok(&b);
    assert!(out(&b).contains("draft heads"), "{}", out(&b));
}
