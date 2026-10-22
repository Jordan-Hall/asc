//! U22 acceptance: `asc commit` and the hooks runner, driven through the built binary on
//! repositories the fixture builds directly (no sibling command is used).

mod support;

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Output;

use asc_core::content::{Body, ingest};
use asc_core::id::{ChangesetId, GitOid, RefTarget};
use asc_core::object::Kind;
use asc_core::path::RefName;
use asc_engine::derive::index_commit_git;
use asc_git::projection::Projection;
use asc_state::state::git_map::RepoGitMap;
use asc_state::state::stage::{self, Leaf, StageRow, StageSet};
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use support::{Fixture, text};

/// A fixture with `f.txt` ("one") and `g.txt` ("keep") committed on `master`, the work tree
/// matching, and an identity configured.
async fn setup() -> (Fixture, ChangesetId) {
    let f = Fixture::new().await;
    fs::write(
        f.sandbox.user.join(".gitconfig"),
        "[user]\n\tname = Tester\n\temail = tester@example.com\n",
    )
    .unwrap();
    let a = f
        .commit(&[], &[("f.txt", b"one"), ("g.txt", b"keep")])
        .await;
    f.set_branch("master", a).await;
    f.checkout_branch("master", Some(a)).await;
    fs::write(f.root.join("f.txt"), "one").unwrap();
    fs::write(f.root.join("g.txt"), "keep").unwrap();
    (f, a)
}

/// Ingests `content` into the store and returns its leaf.
async fn leaf(f: &Fixture, content: &[u8]) -> Leaf {
    let got = ingest(Cursor::new(content)).unwrap();
    let Body::Inline(bytes) = got.body else {
        panic!("small content only");
    };
    f.objects
        .put(&ObjKey::File(got.file), encode_stored(&bytes))
        .await
        .unwrap();
    Leaf {
        kind: Kind::File,
        file: got.file,
        size: got.size,
        git: got.git,
    }
}

async fn stage_content(f: &Fixture, path: &str, content: &[u8]) {
    let l = leaf(f, content).await;
    f.stage(&[StageRow::from_leaf(path.as_bytes().to_vec(), Some(&l))])
        .await;
}

async fn rows(f: &Fixture) -> Vec<StageRow> {
    stage::overlay(&f.db, f.ws, StageSet::Default)
        .await
        .unwrap()
}

async fn master(f: &Fixture) -> ChangesetId {
    let name = RefName::new(b"refs/heads/master".to_vec()).unwrap();
    let entry = Refs::get(&f.db.refs(), f.repo, &name)
        .await
        .unwrap()
        .expect("master");
    match entry.target {
        RefTarget::Changeset(c) => c,
        RefTarget::Tag(_) => panic!("master is a tag"),
    }
}

struct Run {
    code: i32,
    out: String,
    err: String,
}

fn finish(o: Output) -> Run {
    Run {
        code: o.status.code().unwrap_or(-1),
        out: text(&o.stdout),
        err: text(&o.stderr),
    }
}

fn commit(f: &Fixture, args: &[&str]) -> Run {
    finish(f.asc().arg("commit").args(args).output().unwrap())
}

fn commit_env(f: &Fixture, env: &[(&str, &str)], args: &[&str]) -> Run {
    let mut cmd = f.asc();
    cmd.arg("commit").args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    finish(cmd.output().unwrap())
}

fn slashed(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

fn hooks_dir(f: &Fixture) -> PathBuf {
    Projection::open(&f.root)
        .unwrap()
        .common_dir()
        .join("hooks")
}

fn gitdir(f: &Fixture) -> PathBuf {
    Projection::open(&f.root).unwrap().git_dir().to_owned()
}

fn install_hook(f: &Fixture, name: &str, body: &str) {
    let path = hooks_dir(f).join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn log_path(f: &Fixture) -> PathBuf {
    f.sandbox.tmp.path().join("hook.log")
}

fn read_log(f: &Fixture) -> String {
    text(&fs::read(log_path(f)).unwrap_or_default())
}

async fn git_of(f: &Fixture, cs: ChangesetId) -> GitOid {
    f.engine.git_commit_of(cs).await.unwrap()
}

#[tokio::test]
async fn staged_version_is_committed_not_the_edited_file() {
    let (f, a) = setup().await;
    stage_content(&f, "f.txt", b"two").await;
    fs::write(f.root.join("f.txt"), "three").unwrap();
    let r = commit(&f, &["-m", "stage two"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.starts_with("[master "), "{}", r.out);
    assert!(r.out.contains("1 file changed"), "{}", r.out);
    let b = master(&f).await;
    assert_ne!(a, b);
    let cs = f.engine.get_changeset(b).await.unwrap();
    assert_eq!(cs.parents, [a]);
    assert_eq!(
        f.file_leaf(b, "f.txt").await.git,
        leaf(&f, b"two").await.git
    );
    assert_eq!(
        f.file_leaf(b, "g.txt").await.git,
        f.file_leaf(a, "g.txt").await.git
    );
    assert!(rows(&f).await.is_empty(), "the stage is cleared");
    let msg = fs::read_to_string(gitdir(&f).join("COMMIT_EDITMSG")).unwrap();
    assert!(msg.starts_with("stage two"), "{msg}");
}

#[tokio::test]
async fn all_stages_modified_tracked_files() {
    let (f, a) = setup().await;
    fs::write(f.root.join("f.txt"), "changed").unwrap();
    fs::write(f.root.join("untracked.txt"), "x").unwrap();
    let r = commit(&f, &["-a", "-m", "all"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let b = master(&f).await;
    assert_eq!(
        f.file_leaf(b, "f.txt").await.git,
        leaf(&f, b"changed").await.git
    );
    let root = f.engine.get_changeset(b).await.unwrap().root;
    let untracked = asc_core::path::RepoPath::new("untracked.txt").unwrap();
    assert!(f.engine.lookup(root, &untracked).await.unwrap().is_none());
    assert_eq!(f.engine.get_changeset(b).await.unwrap().parents, [a]);
}

#[tokio::test]
async fn nothing_to_commit_exits_one() {
    let (f, a) = setup().await;
    let r = commit(&f, &["-m", "none"]);
    assert_eq!(r.code, 1);
    assert_eq!(
        r.out,
        "On branch master\nnothing to commit, working tree clean\n"
    );
    assert_eq!(master(&f).await, a);
    let r = commit(&f, &["--allow-empty", "-m", "empty"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert_ne!(master(&f).await, a);
}

#[tokio::test]
async fn empty_message_aborts_and_keeps_the_stage() {
    let (f, a) = setup().await;
    stage_content(&f, "f.txt", b"two").await;
    let before = rows(&f).await;
    let r = commit(&f, &["-m", ""]);
    assert_eq!(r.code, 1);
    assert!(
        r.err
            .contains("Aborting commit due to empty commit message."),
        "{}",
        r.err
    );
    assert_eq!(master(&f).await, a);
    assert_eq!(rows(&f).await, before);
    let r = commit(&f, &["--allow-empty-message", "-m", ""]);
    assert_eq!(r.code, 0, "{}", r.err);
}

#[tokio::test]
async fn amend_keeps_parents_and_author() {
    let (f, a) = setup().await;
    stage_content(&f, "f.txt", b"two").await;
    let r = commit(
        &f,
        &[
            "--author",
            "Jane <jane@example.org>",
            "--date",
            "1700000000 +0100",
            "-m",
            "first",
        ],
    );
    assert_eq!(r.code, 0, "{}", r.err);
    let b = master(&f).await;
    let first = f.engine.get_changeset(b).await.unwrap();
    assert_eq!(first.author.name(), b"Jane");
    assert_eq!(
        (first.author.time(), first.author.tz_min()),
        (1_700_000_000, 60)
    );
    assert_eq!(first.committer.name(), b"Tester");

    stage_content(&f, "g.txt", b"amended").await;
    let r = commit(&f, &["--amend", "-m", "second"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let c = master(&f).await;
    let amended = f.engine.get_changeset(c).await.unwrap();
    assert_eq!(amended.parents, [a], "the amend replaces HEAD");
    assert_eq!(amended.author, first.author, "the author survives");
    assert_eq!(amended.message, b"second\n");
    assert_eq!(
        f.file_leaf(c, "f.txt").await.git,
        leaf(&f, b"two").await.git
    );
    assert_eq!(
        f.file_leaf(c, "g.txt").await.git,
        leaf(&f, b"amended").await.git
    );
}

#[tokio::test]
async fn message_file_fixup_and_reflog() {
    let (f, _a) = setup().await;
    let file = f.sandbox.tmp.path().join("msg.txt");
    fs::write(&file, "subject  \n\n\n\nbody line\n\n").unwrap();
    stage_content(&f, "f.txt", b"two").await;
    let r = commit(&f, &["-F", &slashed(&file)]);
    assert_eq!(r.code, 0, "{}", r.err);
    let b = master(&f).await;
    assert_eq!(
        f.engine.get_changeset(b).await.unwrap().message,
        b"subject\n\nbody line\n"
    );

    stage_content(&f, "f.txt", b"three").await;
    let r = commit(&f, &["--fixup", "HEAD"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let c = master(&f).await;
    assert_eq!(
        f.engine.get_changeset(c).await.unwrap().message,
        b"fixup! subject\n"
    );

    let name = RefName::new(b"refs/heads/master".to_vec()).unwrap();
    let log = Refs::log(&f.db.refs(), f.repo, &name, 10).await.unwrap();
    let reasons: Vec<&str> = log.iter().map(|e| e.reason.as_str()).collect();
    assert!(reasons.contains(&"commit: subject"), "{reasons:?}");
    assert!(reasons.contains(&"commit: fixup! subject"), "{reasons:?}");
}

#[tokio::test]
async fn path_commit_takes_the_work_tree_and_keeps_other_staging() {
    let (f, a) = setup().await;
    stage_content(&f, "g.txt", b"staged g").await;
    fs::write(f.root.join("f.txt"), "edited f").unwrap();
    let r = commit(&f, &["-m", "only f", "f.txt"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let b = master(&f).await;
    assert_eq!(
        f.file_leaf(b, "f.txt").await.git,
        leaf(&f, b"edited f").await.git
    );
    assert_eq!(
        f.file_leaf(b, "g.txt").await.git,
        f.file_leaf(a, "g.txt").await.git
    );
    let left = rows(&f).await;
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0].path, b"g.txt");
    assert_eq!(left[0].git, Some(leaf(&f, b"staged g").await.git));
    assert!(!gitdir(&f).join("asc-commit-index").exists());

    let r = commit(&f, &["-m", "nothing", "nosuch.txt"]);
    assert_eq!(r.code, 1);
    assert!(
        r.err
            .contains("pathspec 'nosuch.txt' did not match any file(s) known to git"),
        "{}",
        r.err
    );
    let r = commit(&f, &["--only", "-m", "x"]);
    assert_eq!(r.code, 128);
    assert!(
        r.err
            .contains("No paths with --include/--only does not make sense."),
        "{}",
        r.err
    );
}

#[tokio::test]
async fn include_commits_staged_changes_and_the_named_paths() {
    let (f, _a) = setup().await;
    stage_content(&f, "g.txt", b"staged g").await;
    fs::write(f.root.join("f.txt"), "edited f").unwrap();
    let r = commit(&f, &["-i", "-m", "both", "f.txt"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let b = master(&f).await;
    assert_eq!(
        f.file_leaf(b, "f.txt").await.git,
        leaf(&f, b"edited f").await.git
    );
    assert_eq!(
        f.file_leaf(b, "g.txt").await.git,
        leaf(&f, b"staged g").await.git
    );
    assert!(rows(&f).await.is_empty());
}

#[tokio::test]
async fn signing_stores_the_signature_in_git_gpgsig() {
    let (f, _a) = setup().await;
    let program = f.sandbox.tmp.path().join("fakegpg");
    fs::write(
        &program,
        "#!/bin/sh\ncat >/dev/null\nprintf -- '-----BEGIN PGP SIGNATURE-----\\n\\nabc\\n-----END PGP SIGNATURE-----\\n'\necho '[GNUPG:] SIG_CREATED' >&2\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let cfg = f.sandbox.user.join(".gitconfig");
    let mut text = fs::read_to_string(&cfg).unwrap();
    text.push_str(&format!("[gpg]\n\tprogram = {}\n", slashed(&program)));
    fs::write(&cfg, text).unwrap();
    stage_content(&f, "f.txt", b"two").await;
    let r = commit(&f, &["-S", "-m", "signed"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let b = master(&f).await;
    let cs = f.engine.get_changeset(b).await.unwrap();
    let sig = cs.extra.get(&b"git.gpgsig".to_vec()).expect("signature");
    assert_eq!(
        sig.as_slice(),
        b"-----BEGIN PGP SIGNATURE-----\n\nabc\n-----END PGP SIGNATURE-----"
    );
}

#[tokio::test]
async fn merge_commit_takes_its_second_parent_from_merge_head() {
    let (f, a) = setup().await;
    let other = f.commit(&[], &[("o.txt", b"other")]).await;
    let sink = RepoGitMap {
        db: f.db.clone(),
        repo: f.repo,
    };
    index_commit_git(&f.engine, other, &sink).await.unwrap();
    let dir = gitdir(&f);
    fs::write(
        dir.join("MERGE_HEAD"),
        format!("{}\n", git_of(&f, other).await),
    )
    .unwrap();
    fs::write(dir.join("MERGE_MSG"), "Merge branch 'other'\n").unwrap();
    let r = commit(&f, &["--no-edit"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let m = master(&f).await;
    let cs = f.engine.get_changeset(m).await.unwrap();
    assert_eq!(cs.parents, [a, other]);
    assert_eq!(cs.message, b"Merge branch 'other'\n");
    assert!(!dir.join("MERGE_HEAD").exists() && !dir.join("MERGE_MSG").exists());
}

mod hooks {
    use super::*;

    #[tokio::test]
    async fn a_rejecting_pre_commit_stops_the_commit() {
        let (f, a) = setup().await;
        stage_content(&f, "f.txt", b"two").await;
        let before = rows(&f).await;
        install_hook(&f, "pre-commit", "echo 'lint failed' >&2\nexit 1");
        let r = commit(&f, &["-m", "x"]);
        assert_eq!(r.code, 1);
        assert!(
            r.err.contains("lint failed"),
            "hook stderr is shown: {}",
            r.err
        );
        assert_eq!(master(&f).await, a);
        assert_eq!(rows(&f).await, before);
        let r = commit(&f, &["--no-verify", "-m", "x"]);
        assert_eq!(r.code, 0, "--no-verify skips pre-commit: {}", r.err);
    }

    #[tokio::test]
    async fn failing_pre_commit_after_all_leaves_the_stage_rows_as_they_were() {
        let (f, a) = setup().await;
        stage_content(&f, "g.txt", b"staged g").await;
        fs::write(f.root.join("f.txt"), "edited").unwrap();
        let before = rows(&f).await;
        install_hook(&f, "pre-commit", "exit 1");
        let r = commit(&f, &["-a", "-m", "x"]);
        assert_eq!(r.code, 1);
        assert_eq!(master(&f).await, a);
        assert_eq!(rows(&f).await, before, "-a's staging is rolled back");
        assert!(!gitdir(&f).join("asc-commit-index").exists());
    }

    #[tokio::test]
    async fn a_failing_commit_msg_after_all_rolls_the_stage_back() {
        let (f, a) = setup().await;
        fs::write(f.root.join("f.txt"), "edited").unwrap();
        let before = rows(&f).await;
        install_hook(&f, "commit-msg", "exit 1");
        let r = commit(&f, &["-a", "-m", "x"]);
        assert_eq!(r.code, 1);
        assert_eq!(master(&f).await, a);
        assert_eq!(rows(&f).await, before);
        let r = commit(&f, &["-a", "--no-verify", "-m", "x"]);
        assert_eq!(r.code, 0, "--no-verify skips commit-msg: {}", r.err);
    }

    #[tokio::test]
    async fn commit_msg_can_rewrite_the_message() {
        let (f, _a) = setup().await;
        stage_content(&f, "f.txt", b"two").await;
        install_hook(&f, "commit-msg", "printf 'rewritten by hook\\n' > \"$1\"");
        let r = commit(&f, &["-m", "original"]);
        assert_eq!(r.code, 0, "{}", r.err);
        let b = master(&f).await;
        assert_eq!(
            f.engine.get_changeset(b).await.unwrap().message,
            b"rewritten by hook\n"
        );
    }

    #[tokio::test]
    async fn prepare_commit_msg_gets_git_source_arguments() {
        let (f, _a) = setup().await;
        let log = slashed(&log_path(&f));
        install_hook(
            &f,
            "prepare-commit-msg",
            &format!(
                "echo \"[$2][$3]\" > \"{log}\"\nif [ -z \"$2\" ]; then echo 'from hook' > \"$1\"; fi"
            ),
        );
        let quiet = [("GIT_EDITOR", ":")];

        let r = commit_env(&f, &quiet, &["--allow-empty", "-m", "one"]);
        assert_eq!(r.code, 0, "{}", r.err);
        assert_eq!(read_log(&f), "[message][]\n");

        let head = master(&f).await;
        let sha = git_of(&f, head).await;
        let r = commit_env(&f, &quiet, &["--amend", "--no-edit"]);
        assert_eq!(r.code, 0, "{}", r.err);
        assert_eq!(read_log(&f), format!("[commit][{sha}]\n"));

        let template = f.sandbox.tmp.path().join("tmpl.txt");
        fs::write(&template, "from template\n").unwrap();
        let cfg = f.sandbox.user.join(".gitconfig");
        let mut conf = fs::read_to_string(&cfg).unwrap();
        conf.push_str(&format!("[commit]\n\ttemplate = {}\n", slashed(&template)));
        fs::write(&cfg, conf).unwrap();
        let r = commit_env(&f, &quiet, &["--allow-empty"]);
        assert_eq!(r.code, 0, "{}", r.err);
        assert_eq!(read_log(&f), "[template][]\n");
        let b = master(&f).await;
        assert_eq!(
            f.engine.get_changeset(b).await.unwrap().message,
            b"from template\n"
        );

        fs::write(
            &cfg,
            "[user]\n\tname = Tester\n\temail = tester@example.com\n",
        )
        .unwrap();
        let r = commit_env(&f, &quiet, &["--allow-empty"]);
        assert_eq!(r.code, 0, "{}", r.err);
        assert_eq!(read_log(&f), "[][]\n");
        let b = master(&f).await;
        assert_eq!(
            f.engine.get_changeset(b).await.unwrap().message,
            b"from hook\n"
        );
    }

    #[tokio::test]
    async fn prepare_commit_msg_source_is_merge_during_a_merge() {
        let (f, _a) = setup().await;
        let other = f.commit(&[], &[("o.txt", b"other")]).await;
        let sink = RepoGitMap {
            db: f.db.clone(),
            repo: f.repo,
        };
        index_commit_git(&f.engine, other, &sink).await.unwrap();
        let dir = gitdir(&f);
        fs::write(
            dir.join("MERGE_HEAD"),
            format!("{}\n", git_of(&f, other).await),
        )
        .unwrap();
        fs::write(dir.join("MERGE_MSG"), "Merge branch 'other'\n").unwrap();
        let log = slashed(&log_path(&f));
        install_hook(
            &f,
            "prepare-commit-msg",
            &format!("echo \"[$2]\" > \"{log}\""),
        );
        let r = commit_env(&f, &[("GIT_EDITOR", ":")], &[]);
        assert_eq!(r.code, 0, "{}", r.err);
        assert_eq!(read_log(&f), "[merge]\n");
    }

    #[tokio::test]
    async fn amend_runs_post_rewrite_with_git_ids() {
        let (f, a) = setup().await;
        let log = slashed(&log_path(&f));
        install_hook(
            &f,
            "post-rewrite",
            &format!("echo \"$1\" > \"{log}\"\ncat >> \"{log}\""),
        );
        install_hook(
            &f,
            "post-commit",
            &format!("echo post-commit >> \"{log}.pc\""),
        );
        stage_content(&f, "f.txt", b"two").await;
        let r = commit(&f, &["-m", "first"]);
        assert_eq!(r.code, 0, "{}", r.err);
        assert_eq!(read_log(&f), "", "a plain commit does not rewrite");
        let first = master(&f).await;
        let r = commit(&f, &["--amend", "--allow-empty", "-m", "second"]);
        assert_eq!(r.code, 0, "{}", r.err);
        let second = master(&f).await;
        assert_ne!(first, second);
        assert_eq!(f.engine.get_changeset(second).await.unwrap().parents, [a]);
        let want = format!(
            "amend\n{} {}\n",
            git_of(&f, first).await,
            git_of(&f, second).await
        );
        assert_eq!(read_log(&f), want);
        let pc = text(&fs::read(format!("{}.pc", log_path(&f).display())).unwrap());
        assert_eq!(pc, "post-commit\npost-commit\n");
    }

    #[tokio::test]
    async fn reference_transaction_sees_prepared_then_committed() {
        let (f, a) = setup().await;
        stage_content(&f, "f.txt", b"two").await;
        let log = slashed(&log_path(&f));
        install_hook(
            &f,
            "reference-transaction",
            &format!("echo \"$1\" >> \"{log}\"\ncat >> \"{log}\""),
        );
        let r = commit(&f, &["-m", "x"]);
        assert_eq!(r.code, 0, "{}", r.err);
        let b = master(&f).await;
        let line = format!(
            "{} {} refs/heads/master\n",
            git_of(&f, a).await,
            git_of(&f, b).await
        );
        assert_eq!(read_log(&f), format!("prepared\n{line}committed\n{line}"));
    }

    #[tokio::test]
    async fn a_rejecting_reference_transaction_aborts_the_ref_move() {
        let (f, a) = setup().await;
        stage_content(&f, "f.txt", b"two").await;
        let log = slashed(&log_path(&f));
        install_hook(
            &f,
            "reference-transaction",
            &format!(
                "echo \"$1\" >> \"{log}\"\ncat > /dev/null\nif [ \"$1\" = prepared ]; then exit 1; fi"
            ),
        );
        let r = commit(&f, &["-m", "x"]);
        assert_ne!(r.code, 0);
        assert_eq!(master(&f).await, a);
        assert_eq!(read_log(&f), "prepared\naborted\n");
    }

    #[tokio::test]
    async fn hooks_get_the_index_file_prefix_and_editor_environment() {
        let (f, _a) = setup().await;
        fs::create_dir_all(f.root.join("sub")).unwrap();
        let log = slashed(&log_path(&f));
        install_hook(
            &f,
            "pre-commit",
            &format!(
                "echo \"$GIT_INDEX_FILE|$GIT_EDITOR|$GIT_PREFIX|$GIT_REFLOG_ACTION|$(pwd)\" >> \"{log}\""
            ),
        );
        stage_content(&f, "f.txt", b"two").await;
        let mut cmd = f.asc();
        cmd.current_dir(f.root.join("sub"))
            .args(["commit", "-m", "x"]);
        let r = finish(cmd.output().unwrap());
        assert_eq!(r.code, 0, "{}", r.err);
        let dir = slashed(&gitdir(&f)).to_lowercase();
        let line = read_log(&f).to_lowercase();
        let parts: Vec<&str> = line.trim_end().split('|').collect();
        assert_eq!(parts[0].replace('\\', "/"), format!("{dir}/index"));
        assert_eq!(parts[1], ":");
        assert_eq!(parts[2], "sub/");
        assert_eq!(parts[3], "commit");
        assert!(
            parts[4].ends_with("/work") || parts[4].ends_with("work"),
            "{}",
            parts[4]
        );

        // A path commit hands hooks the temporary set's file.
        fs::write(f.root.join("g.txt"), "edited").unwrap();
        fs::write(log_path(&f), "").unwrap();
        let r = commit(&f, &["-m", "y", "g.txt"]);
        assert_eq!(r.code, 0, "{}", r.err);
        let line = read_log(&f).to_lowercase();
        let index = line.split('|').next().unwrap().replace('\\', "/");
        assert_eq!(index, format!("{dir}/asc-commit-index"));
    }

    #[tokio::test]
    async fn the_temporary_index_file_exists_while_the_hook_runs() {
        let (f, _a) = setup().await;
        fs::write(f.root.join("g.txt"), "edited").unwrap();
        let log = slashed(&log_path(&f));
        install_hook(
            &f,
            "pre-commit",
            &format!(
                "if [ -s \"$GIT_INDEX_FILE\" ]; then echo present > \"{log}\"; else echo missing > \"{log}\"; fi"
            ),
        );
        let r = commit(&f, &["-m", "y", "g.txt"]);
        assert_eq!(r.code, 0, "{}", r.err);
        assert_eq!(read_log(&f), "present\n");
    }

    #[tokio::test]
    async fn a_relative_hooks_path_resolves_against_the_work_tree() {
        let (f, a) = setup().await;
        let custom = f.root.join(".husky");
        fs::create_dir_all(&custom).unwrap();
        fs::write(custom.join("pre-commit"), "#!/bin/sh\nexit 1\n").unwrap();
        let common = Projection::open(&f.root).unwrap().common_dir().to_owned();
        let cfg = common.join("config");
        let mut conf = fs::read_to_string(&cfg).unwrap_or_default();
        conf.push_str("\n[core]\n\thooksPath = .husky\n");
        fs::write(&cfg, conf).unwrap();
        stage_content(&f, "f.txt", b"two").await;
        let r = commit(&f, &["-m", "x"]);
        assert_eq!(r.code, 1, "{}", r.err);
        assert_eq!(master(&f).await, a);
    }
}

/// Makes `link` a directory symlink (Unix) or a junction (Windows, where a symlink needs a
/// privilege) to `target`; false when the OS refuses.
fn link_dir(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(target, link).is_ok();
    #[cfg(windows)]
    let made = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .is_ok_and(|o| o.status.success());
    #[cfg(not(any(unix, windows)))]
    let made = false;
    made
}

/// A tracked path under a linked directory is outside the work tree (git's
/// has_symlink_leading_path): `commit <path>` and `commit -a` never read it.
#[tokio::test]
async fn commit_does_not_read_through_a_linked_directory() {
    for args in [&["-m", "p", "d/x"][..], &["-a", "-m", "all"]] {
        let f = Fixture::new().await;
        fs::write(
            f.sandbox.user.join(".gitconfig"),
            "[user]\n\tname = Tester\n\temail = tester@example.com\n",
        )
        .unwrap();
        let a = f.commit(&[], &[("d/x", b"one"), ("g.txt", b"keep")]).await;
        f.set_branch("master", a).await;
        f.checkout_branch("master", Some(a)).await;
        fs::write(f.root.join("g.txt"), "keep").unwrap();
        let outside = f.sandbox.dir("outside");
        fs::write(outside.join("x"), "SECRET").unwrap();
        assert!(link_dir(&outside, &f.root.join("d")), "cannot make a directory link here");

        let secret = leaf(&f, b"SECRET").await.git;
        let r = commit(&f, args);
        assert!(r.code == 0 || r.code == 1, "{args:?}: {}", r.err);
        let tip = master(&f).await;
        let root = f.engine.get_changeset(tip).await.unwrap().root;
        let path = asc_core::path::RepoPath::new("d/x").unwrap();
        if let Some(entry) = f.engine.lookup(root, &path).await.unwrap() {
            assert_ne!(entry.git, secret, "{args:?} committed outside content");
        }
        assert_eq!(fs::read(outside.join("x")).unwrap(), b"SECRET");
    }
}
