//! U24d acceptance: `rev-parse`, `rev-list`, `merge-base`, `describe` and `reflog` through the
//! built binary, compared with real git 2.55.
//!
//! Scenarios are built twice from one description (an asc fixture through the engine, a real git
//! repository through `fast-import`), so both sides share every commit id and the outputs are
//! compared byte for byte. `reflog` and the repository-location queries have no git twin (the
//! ref log is asc's `state.db`, the gitdir is the synthetic one) and assert their own text.

mod support;

use std::collections::HashMap;
use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use asc_core::codec::Enveloped as _;
use asc_core::content::{Body, ingest};
use asc_core::git::ObjectKind;
use asc_core::id::{ChangesetId, RefTarget, TagId};
use asc_core::object::SortedMap;
use asc_core::object::{Change, Kind, TagRecord};
use asc_core::path::{RefName, RepoPath};
use asc_core::sig::Signature;
use asc_engine::Engine;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_engine::derive::GitMapRow;
use asc_state::state::git_map;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
use asc_store::sqlrefs::SqlRefs;
use asc_store::storeset::StoreSet;
use support::{Fixture, text};

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run(cmd: &mut Command) -> Out {
    let out = cmd.output().expect("spawn");
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: text(&out.stdout),
        stderr: text(&out.stderr),
    }
}

/// Real git in `dir`: hermetic, UTC, no pager.
fn git_cmd(dir: &Path, home: &Path) -> Command {
    let mut c = asc_git::realgit::pinned_command(); // lint:allow-raw-git
    c.current_dir(dir)
        .env_remove("GIT_DIR")
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_PAGER", "cat")
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .env("NO_COLOR", "1")
        .args(["-c", "core.autocrlf=false", "-c", "core.quotePath=true"]);
    c
}

#[derive(Clone, Copy)]
struct Who {
    name: &'static str,
    email: &'static str,
    time: i64,
    tz: i16,
}

impl Who {
    fn at(time: i64) -> Self {
        Self {
            name: "Test",
            email: "test@example.com",
            time,
            tz: 0,
        }
    }

    fn tz_text(&self) -> String {
        let a = self.tz.abs();
        format!(
            "{}{:02}{:02}",
            if self.tz < 0 { '-' } else { '+' },
            a / 60,
            a % 60
        )
    }

    fn sig(&self) -> Signature {
        Signature::new(
            self.name.as_bytes().to_vec(),
            self.email.as_bytes().to_vec(),
            self.time,
            self.tz,
        )
        .unwrap()
    }
}

/// One commit: its full file snapshot, parents (by name), identities and message.
struct Step {
    name: String,
    parents: Vec<String>,
    files: Vec<(String, String)>,
    author: Who,
    committer: Who,
    msg: String,
}

fn step(name: &str, parents: &[&str], files: &[(&str, &str)], time: i64, msg: &str) -> Step {
    Step {
        name: name.into(),
        parents: parents.iter().map(|p| (*p).to_owned()).collect(),
        files: files
            .iter()
            .map(|(p, c)| ((*p).to_owned(), (*c).to_owned()))
            .collect(),
        author: Who::at(time),
        committer: Who::at(time),
        msg: msg.into(),
    }
}

/// Both repositories of a scenario.
struct Built {
    f: Fixture,
    git_dir: PathBuf,
    ids: HashMap<String, ChangesetId>,
    shas: HashMap<String, String>,
}

async fn set_ref_asc(f: &Fixture, name: &str, target: RefTarget) {
    let refs = f.db.refs();
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name: RefName::new(name.as_bytes().to_vec()).unwrap(),
            expect: Expect::Any,
            new: Some(target),
            expires_at: None,
        }],
        actor: Actor {
            id: "fixture".into(),
            via: "test".into(),
        },
        reason: "fixture".into(),
        op_id: uuid::Uuid::now_v7(),
    };
    Refs::update(&refs, f.repo, &txn).await.unwrap();
}

async fn asc_commit(f: &Fixture, s: &Step, parents: Vec<ChangesetId>) -> ChangesetId {
    let mut changes = Vec::new();
    for (path, content) in &s.files {
        let got = ingest(Cursor::new(content.as_bytes())).unwrap();
        let Body::Inline(bytes) = got.body else {
            panic!("scenario files are small")
        };
        f.objects
            .put(&ObjKey::File(got.file), encode_stored(&bytes))
            .await
            .unwrap();
        changes.push((
            RepoPath::new(path.as_str()).unwrap(),
            Change::write(Kind::File, got.file, got.size, got.git, None).unwrap(),
        ));
    }
    let root = f
        .engine
        .apply(Engine::<StoreSet, SqlRefs>::empty_tree_id(), &changes)
        .await
        .unwrap();
    f.engine
        .create_changeset(
            NewChangeset {
                parents,
                author: s.author.sig(),
                committer: s.committer.sig(),
                message: s.msg.as_bytes().to_vec(),
                extra: Vec::new(),
                root,
                copies: Vec::new(),
            },
            PathPolicy::Native { windows_safe: true },
        )
        .await
        .unwrap()
}

/// Builds `steps` in a git repository with one `fast-import` stream; returns each step's commit id.
fn git_build(dir: &Path, home: &Path, steps: &[Step]) -> HashMap<String, String> {
    let init = run(git_cmd(dir, home).args(["init", "-q", "-b", "master"]));
    assert_eq!(init.code, 0, "{}", init.stderr);
    let mut stream: Vec<u8> = Vec::new();
    let mark: HashMap<&str, usize> = steps
        .iter()
        .enumerate()
        .map(|(i, s)| (s.name.as_str(), i + 1))
        .collect();
    for (i, s) in steps.iter().enumerate() {
        stream.extend_from_slice(b"commit refs/heads/work\n");
        stream.extend_from_slice(format!("mark :{}\n", i + 1).as_bytes());
        for (k, w) in [("author", s.author), ("committer", s.committer)] {
            stream.extend_from_slice(
                format!("{k} {} <{}> {} {}\n", w.name, w.email, w.time, w.tz_text()).as_bytes(),
            );
        }
        stream.extend_from_slice(format!("data {}\n{}\n", s.msg.len(), s.msg).as_bytes());
        if let Some(first) = s.parents.first() {
            stream.extend_from_slice(format!("from :{}\n", mark[first.as_str()]).as_bytes());
        }
        for p in s.parents.iter().skip(1) {
            stream.extend_from_slice(format!("merge :{}\n", mark[p.as_str()]).as_bytes());
        }
        stream.extend_from_slice(b"deleteall\n");
        for (path, content) in &s.files {
            stream.extend_from_slice(
                format!(
                    "M 100644 inline {path}\ndata {}\n{content}\n",
                    content.len()
                )
                .as_bytes(),
            );
        }
        stream.push(b'\n');
    }
    let marks = dir.join("marks.txt");
    let mut child = git_cmd(dir, home)
        .args([
            "fast-import",
            "--quiet",
            &format!("--export-marks={}", marks.display()),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&stream).unwrap();
    let done = child.wait_with_output().unwrap();
    assert!(
        done.status.success(),
        "fast-import: {}",
        String::from_utf8_lossy(&done.stderr)
    );
    let table = fs::read_to_string(&marks).unwrap();
    let by_mark: HashMap<usize, String> = table
        .lines()
        .filter_map(|l| {
            let (m, sha) = l.split_once(' ')?;
            Some((m.trim_start_matches(':').parse().ok()?, sha.to_owned()))
        })
        .collect();
    fs::remove_file(&marks).unwrap();
    steps
        .iter()
        .enumerate()
        .map(|(i, s)| (s.name.clone(), by_mark[&(i + 1)].clone()))
        .collect()
}

impl Built {
    /// Builds `steps`; `head` is the step `master` points at.
    async fn new(steps: Vec<Step>, head: &str) -> Self {
        let f = Fixture::new().await;
        let git_dir = f.sandbox.dir("gitrepo");
        let shas = git_build(&git_dir, &f.sandbox.user, &steps);
        let mut ids: HashMap<String, ChangesetId> = HashMap::new();
        let mut rows = Vec::new();
        for s in &steps {
            let parents = s.parents.iter().map(|p| ids[p]).collect();
            let id = asc_commit(&f, s, parents).await;
            let git = f.engine.git_commit_of(id).await.unwrap();
            assert_eq!(
                git.to_string(),
                shas[&s.name],
                "commit {} must have the same id in both repositories",
                s.name
            );
            rows.push(GitMapRow {
                git,
                kind: ObjectKind::Commit,
                asc: *id.as_bytes(),
            });
            ids.insert(s.name.clone(), id);
        }
        git_map::insert_many(&f.db, f.repo, &rows).await.unwrap();
        let b = Self {
            f,
            git_dir,
            ids,
            shas,
        };
        b.branch("master", head).await;
        let r = b.git(&["update-ref", "-d", "refs/heads/work"]);
        assert_eq!(r.code, 0, "{}", r.stderr);
        b.f.checkout_branch("master", Some(b.ids[head])).await;
        let r = b.git(&["symbolic-ref", "HEAD", "refs/heads/master"]);
        assert_eq!(r.code, 0, "{}", r.stderr);
        b
    }

    fn git(&self, args: &[&str]) -> Out {
        run(git_cmd(&self.git_dir, &self.f.sandbox.user).args(args))
    }

    fn git_env(&self, env: &[(&str, &str)], args: &[&str]) -> Out {
        let mut c = git_cmd(&self.git_dir, &self.f.sandbox.user);
        c.envs(env.iter().copied()).args(args);
        run(&mut c)
    }

    fn asc(&self, args: &[&str]) -> Out {
        run(self.f.asc().args(args))
    }

    /// Points `refs/<full>` at step `at` in both repositories.
    async fn reference(&self, full: &str, at: &str) {
        set_ref_asc(&self.f, full, RefTarget::Changeset(self.ids[at])).await;
        let r = self.git(&["update-ref", full, &self.shas[at]]);
        assert_eq!(r.code, 0, "{}", r.stderr);
    }

    async fn branch(&self, name: &str, at: &str) {
        self.reference(&format!("refs/heads/{name}"), at).await;
    }

    async fn tag(&self, name: &str, at: &str) {
        self.reference(&format!("refs/tags/{name}"), at).await;
    }

    /// An annotated tag by the same tagger in both repositories.
    async fn annotated_tag(&self, name: &str, at: &str, message: &str, time: i64) {
        let tagger = Who {
            name: "Tagger",
            email: "tagger@example.com",
            time,
            tz: 60,
        };
        let rec = TagRecord {
            target: self.ids[at],
            name: name.as_bytes().to_vec(),
            tagger: Some(tagger.sig()),
            message: message.as_bytes().to_vec(),
            extra: SortedMap::new(vec![]).unwrap(),
        };
        let id: TagId = rec.id();
        self.f
            .objects
            .put(&ObjKey::Tag(id), encode_stored(&rec.to_envelope()))
            .await
            .unwrap();
        set_ref_asc(&self.f, &format!("refs/tags/{name}"), RefTarget::Tag(id)).await;
        let when = format!("{time} +0100");
        let r = self.git_env(
            &[
                ("GIT_COMMITTER_NAME", tagger.name),
                ("GIT_COMMITTER_EMAIL", tagger.email),
                ("GIT_COMMITTER_DATE", &when),
            ],
            &["tag", "-a", name, &self.shas[at], "-m", message.trim_end()],
        );
        assert_eq!(r.code, 0, "{}", r.stderr);
    }

    /// `asc <args>` equals `git <args>`: stdout and exit code.
    fn same(&self, args: &[&str]) {
        let want = self.git(args);
        let got = self.asc(args);
        assert_eq!(
            got.code, want.code,
            "exit code of {args:?}\nasc stderr: {}\ngit stderr: {}",
            got.stderr, want.stderr
        );
        assert_eq!(
            got.stdout, want.stdout,
            "stdout of {args:?}\n--- asc stderr:\n{}",
            got.stderr
        );
        if want.code != 0 {
            let normal = |s: &str| s.replace("'git <command>", "'asc <command>");
            assert_eq!(got.stderr, normal(&want.stderr), "stderr of {args:?}");
        }
    }

    /// `same` for `command` with each argument list.
    fn same_all(&self, command: &str, lists: &[&[&str]]) {
        for list in lists {
            let args: Vec<&str> = std::iter::once(command)
                .chain(list.iter().copied())
                .collect();
            self.same(&args);
        }
    }

    /// Stdout and exit code only (usage texts differ in wording between tools).
    fn same_out(&self, args: &[&str]) {
        let want = self.git(args);
        let got = self.asc(args);
        assert_eq!(
            got.code, want.code,
            "exit code of {args:?}\nasc stderr: {}",
            got.stderr
        );
        assert_eq!(got.stdout, want.stdout, "stdout of {args:?}");
    }

    /// The git sha of step `name` with `suffix` (`~1`, `^2`).
    fn sha(&self, name: &str, suffix: &str) -> String {
        format!("{}{suffix}", self.shas[name])
    }
}

const T0: i64 = 1_600_000_000;
const DAY: i64 = 86_400;

/// A history with a merge, a rename, tabs, trailers and several identities.
fn history() -> Vec<Step> {
    let a = |s: &mut Step| {
        s.author = Who {
            name: "Ann Author",
            email: "ann@example.org",
            time: s.author.time,
            tz: 0,
        };
    };
    let mut c1 = step(
        "c1",
        &[],
        &[("a.txt", "one\n"), ("sub/b.txt", "b1\n")],
        T0,
        "Add a and b\n\nThe first commit with a body.\nSecond line of the body.\n",
    );
    a(&mut c1);
    c1.committer = c1.author;
    let mut c2 = step(
        "c2",
        &["c1"],
        &[
            ("a.txt", "one\ntwo\n"),
            ("sub/b.txt", "b1\n"),
            ("c.txt", "c\n"),
        ],
        T0 + DAY,
        "Second change\n",
    );
    c2.author = Who {
        name: "Bob Builder",
        email: "bob@example.com",
        time: T0 + DAY,
        tz: -300,
    };
    c2.committer = Who {
        name: "Cy Committer",
        email: "cy@example.net",
        time: T0 + DAY + 600,
        tz: 330,
    };
    let mut c3 = step(
        "c3",
        &["c2"],
        &[
            ("a.txt", "one\ntwo\n"),
            ("sub/b.txt", "b1\nb2\n"),
            ("c.txt", "c\n"),
        ],
        T0 + 2 * DAY,
        "Side branch work\n\nSigned-off-by: Side <side@example.org>\n",
    );
    c3.author = Who {
        name: "Side Dev",
        email: "side@example.org",
        time: T0 + 2 * DAY,
        tz: 120,
    };
    c3.committer = c3.author;
    let c4 = step(
        "c4",
        &["c2"],
        &[
            ("a.txt", "one\ntwo\nthree\n"),
            ("sub/b.txt", "b1\n"),
            ("c.txt", "c\n"),
        ],
        T0 + 3 * DAY,
        "Main work\ncontinued subject line\n\nBody paragraph\n\n\nAfter blanks\twith a tab  \n  indented\n",
    );
    let c5 = step(
        "c5",
        &["c4", "c3"],
        &[
            ("a.txt", "one\ntwo\nthree\n"),
            ("sub/b.txt", "b1\nb2\n"),
            ("c.txt", "c\n"),
        ],
        T0 + 4 * DAY,
        "Merge branch 'side'\n",
    );
    let c6 = step(
        "c6",
        &["c5"],
        &[
            ("a.txt", "one\ntwo\nthree\n"),
            ("sub/b.txt", "b1\nb2\n"),
            ("d.txt", "c\n"),
        ],
        T0 + 5 * DAY,
        "Rename c to d\n\nBug: 42\nSigned-off-by: Cy <cy@example.net>\nReviewed-by: Dee <dee@example.org>\n",
    );
    let big: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    let c7 = step(
        "c7",
        &["c6"],
        &[
            ("a.txt", "ONE\ntwo\nthree\n"),
            ("sub/b.txt", "b1\nb2\n"),
            ("d.txt", "c\n"),
            ("big.txt", &big),
        ],
        T0 + 6 * DAY,
        "Third: uppercase a, add big\n",
    );
    vec![c1, c2, c3, c4, c5, c6, c7]
}

async fn standard() -> Built {
    let b = Built::new(history(), "c7").await;
    b.branch("side", "c3").await;
    b.tag("v1", "c2").await;
    b.reference("refs/remotes/origin/master", "c6").await;
    b.annotated_tag(
        "v2",
        "c4",
        "Release two\n\nWith notes.\n",
        T0 + 3 * DAY + 60,
    )
    .await;
    b
}

// ------------------------------------------------------------------------------- rev-parse

#[tokio::test]
async fn rev_parse_names_match_git() {
    let b = standard().await;
    let full = b.sha("c4", "");
    let back = b.sha("c7", "~2");
    let second = b.sha("c5", "^2");
    let range = format!("{}..{}", b.shas["c2"], b.shas["c4"]);
    b.same_all(
        "rev-parse",
        &[
            &["HEAD"],
            &["master"],
            &["HEAD~1"],
            &["HEAD^"],
            &["HEAD~3^2"],
            &["HEAD~3^{commit}"],
            &["HEAD^{tree}"],
            &["HEAD^{}"],
            &["side"],
            &["origin/master"],
            &["refs/heads/side"],
            &["heads/side"],
            &["v1"],
            &["v2"],
            &["v2^{}"],
            &["v2^{commit}"],
            &["v2^{tree}"],
            &["v2~1"],
            &["HEAD:a.txt"],
            &["HEAD:sub"],
            &["HEAD:sub/b.txt"],
            &["HEAD:"],
            &["HEAD~1:d.txt"],
            &[full.as_str()],
            &[back.as_str()],
            &[second.as_str()],
            &["HEAD", "side", "v1"],
            &[range.as_str()],
        ],
    );
}

#[tokio::test]
async fn rev_parse_ranges_match_git() {
    let b = standard().await;
    b.same_all(
        "rev-parse",
        &[
            &["HEAD..side"],
            &["side..HEAD"],
            &["HEAD...side"],
            &["side...HEAD"],
            &["^side"],
            &["^side", "HEAD"],
            &["HEAD^@"],
            &["HEAD~2^@"],
            &["HEAD~2^!"],
            &["HEAD^!"],
            &["v2..master"],
        ],
    );
}

#[tokio::test]
async fn rev_parse_output_modes_match_git() {
    let b = standard().await;
    b.same_all(
        "rev-parse",
        &[
            &["--short", "HEAD"],
            &["--short=10", "HEAD"],
            &["--short=4", "HEAD~1"],
            &["--short", "HEAD:a.txt"],
            &["--short", "HEAD^{tree}"],
            &["--abbrev-ref", "HEAD"],
            &["--abbrev-ref", "side"],
            &["--abbrev-ref", "v1"],
            &["--abbrev-ref", "origin/master"],
            &["--abbrev-ref", "refs/heads/side"],
            &["--abbrev-ref=strict", "HEAD"],
            &["--abbrev-ref", "HEAD~1"],
            &["--abbrev-ref", "master", "side"],
            &["--symbolic-full-name", "HEAD"],
            &["--symbolic-full-name", "side"],
            &["--symbolic-full-name", "v1"],
            &["--symbolic-full-name", "HEAD~1"],
            &["--symbolic", "HEAD~1"],
            &["--symbolic", "HEAD..side"],
            &["--all"],
            &["--branches"],
            &["--tags"],
            &["--remotes"],
            &["--abbrev-ref", "--all"],
        ],
    );
}

#[tokio::test]
async fn rev_parse_verify_matches_git() {
    let b = standard().await;
    b.same_all(
        "rev-parse",
        &[
            &["--verify", "HEAD"],
            &["--verify", "v2"],
            &["--verify", "HEAD^{tree}"],
            &["--verify", "--short", "HEAD"],
            &["--verify", "-q", "HEAD"],
            &["--verify", "nothere"],
            &["--verify", "-q", "nothere"],
            &["-q", "--verify", "HEAD^2"],
            &["--verify"],
            &["--verify", "HEAD", "side"],
            &["--verify", "HEAD..side"],
        ],
    );
}

#[tokio::test]
async fn rev_parse_errors_and_paths_match_git() {
    let b = standard().await;
    for dir in [b.f.root.as_path(), b.git_dir.as_path()] {
        fs::write(dir.join("loose.txt"), "x").unwrap();
    }
    b.same_all(
        "rev-parse",
        &[
            &["nothere"],
            &["HEAD~99"],
            &["HEAD^9"],
            &["loose.txt"],
            &["HEAD", "loose.txt"],
            &["HEAD", "--", "loose.txt"],
            &["--end-of-options", "HEAD"],
            &["HEAD", "nothere", "side"],
        ],
    );
}

#[tokio::test]
async fn rev_parse_unborn_head_matches_git() {
    let f = Fixture::new().await;
    let git_dir = f.sandbox.dir("gitrepo");
    let init = run(git_cmd(&git_dir, &f.sandbox.user).args(["init", "-q", "-b", "master"]));
    assert_eq!(init.code, 0, "{}", init.stderr);
    for args in [
        &["rev-parse", "HEAD"][..],
        &["rev-parse", "--verify", "HEAD"],
        &["rev-parse", "--abbrev-ref", "HEAD"],
        &["rev-parse", "-q", "--verify", "HEAD"],
    ] {
        let want = run(git_cmd(&git_dir, &f.sandbox.user).args(args));
        let got = run(f.asc().args(args));
        let normal = |s: &str| s.replace("'git <command>", "'asc <command>");
        assert_eq!(
            (got.code, got.stdout.as_str(), got.stderr.as_str()),
            (
                want.code,
                want.stdout.as_str(),
                normal(&want.stderr).as_str()
            ),
            "{args:?}"
        );
    }
}

#[tokio::test]
async fn rev_parse_prints_asc_ids_on_request() {
    let b = standard().await;
    let head = b.asc(&["rev-parse", "--asc", "HEAD"]);
    assert_eq!(head.code, 0, "{}", head.stderr);
    assert_eq!(head.stdout.trim(), b.ids["c7"].to_string());
    let both = b.asc(&[
        "rev-parse",
        "--asc",
        "HEAD",
        "--git",
        "HEAD",
        "--asc",
        "--short",
        "HEAD~1",
    ]);
    let lines: Vec<&str> = both.stdout.lines().collect();
    assert_eq!(
        lines,
        [
            b.ids["c7"].to_string(),
            b.shas["c7"].clone(),
            b.ids["c6"].to_string()[..12].to_owned()
        ]
    );
    let tree = b.asc(&["rev-parse", "--asc", "HEAD^{tree}"]);
    assert_eq!(tree.stdout.trim().len(), 64, "{}", tree.stderr);
}

#[tokio::test]
async fn rev_parse_locates_the_repository() {
    let b = standard().await;
    let root = b.f.root.canonicalize().unwrap();
    let norm = |p: &Path| {
        p.display()
            .to_string()
            .replace('\\', "/")
            .trim_start_matches("//?/")
            .to_owned()
    };
    let top = b.asc(&["rev-parse", "--show-toplevel"]);
    assert_eq!(top.stdout.trim(), norm(&root), "{}", top.stderr);
    let gitdir = b.asc(&["rev-parse", "--git-dir"]).stdout.trim().to_owned();
    assert!(Path::new(&gitdir).join("HEAD").is_file(), "{gitdir}");
    assert_eq!(
        b.asc(&["rev-parse", "--absolute-git-dir"]).stdout.trim(),
        gitdir
    );
    let common = b
        .asc(&["rev-parse", "--git-common-dir"])
        .stdout
        .trim()
        .to_owned();
    assert!(
        Path::new(&common).is_dir() && common.ends_with("/.git"),
        "{common}"
    );
    for (flag, want) in [
        ("--is-inside-work-tree", "true\n"),
        ("--is-inside-git-dir", "false\n"),
        ("--is-bare-repository", "false\n"),
        ("--is-shallow-repository", "false\n"),
        ("--show-prefix", "\n"),
        ("--show-cdup", "\n"),
        ("--show-object-format", "sha1\n"),
    ] {
        assert_eq!(b.asc(&["rev-parse", flag]).stdout, want, "{flag}");
    }
    let sub = b.f.root.join("sub").join("deep");
    fs::create_dir_all(&sub).unwrap();
    let in_sub = |flag: &str| {
        text(
            &b.f.sandbox
                .asc(&sub)
                .args(["rev-parse", flag])
                .output()
                .unwrap()
                .stdout,
        )
    };
    assert_eq!(in_sub("--show-prefix"), "sub/deep/\n");
    assert_eq!(in_sub("--show-cdup"), "../../\n");
    assert_eq!(in_sub("--show-toplevel").trim(), norm(&root));
    assert_eq!(in_sub("--is-inside-work-tree"), "true\n");
}

#[tokio::test]
async fn rev_parse_outside_a_repository_fails() {
    let sandbox = support::Sandbox::new();
    let dir = sandbox.dir("nowhere");
    let out = sandbox.run(&dir, &["rev-parse", "--show-toplevel"]);
    assert_eq!(out.status.code(), Some(128));
    assert!(
        text(&out.stderr).starts_with("fatal: not a git repository"),
        "{}",
        text(&out.stderr)
    );
}

// -------------------------------------------------------------------------------- rev-list

#[tokio::test]
async fn rev_list_walks_match_git() {
    let b = standard().await;
    let range = format!("{}..{}", b.shas["c2"], b.shas["c6"]);
    b.same_all(
        "rev-list",
        &[
            &["HEAD"],
            &["master"],
            &["side"],
            &["--all"],
            &["--branches"],
            &["--tags"],
            &["--remotes"],
            &["HEAD~2..HEAD"],
            &["side..master"],
            &["master..side"],
            &["^side", "master"],
            &["master", "--not", "side"],
            &["master", "^side", "^v1"],
            &[range.as_str()],
            &["--reverse", "HEAD"],
            &["--first-parent", "HEAD"],
            &["--no-merges", "HEAD"],
            &["--merges", "HEAD"],
            &["--min-parents=2", "HEAD"],
            &["--max-parents=0", "HEAD"],
            &["-n2", "HEAD"],
            &["-3", "HEAD"],
            &["--max-count=3", "HEAD"],
            &["-n", "2", "HEAD"],
            &["--skip=2", "HEAD"],
            &["--skip=1", "-n2", "HEAD"],
            &["--topo-order", "HEAD"],
            &["--date-order", "HEAD"],
            &["--author-date-order", "HEAD"],
            &["--author=Ann", "HEAD"],
            &["--committer=Cy", "HEAD"],
            &["--grep=Merge", "HEAD"],
            &["--grep=merge", "-i", "HEAD"],
            &["--since=2020-09-15T13:00:00Z", "HEAD"],
            &["--until=2020-09-15T13:00:00Z", "HEAD"],
            &["HEAD", "--", "d.txt"],
            &["HEAD", "--", "a.txt"],
        ],
    );
}

#[tokio::test]
async fn rev_list_output_forms_match_git() {
    let b = standard().await;
    b.same_all(
        "rev-list",
        &[
            &["--count", "HEAD"],
            &["--count", "side..master"],
            &["--count", "--all"],
            &["--count", "-n2", "HEAD"],
            &["--count", "HEAD", "--", "d.txt"],
            &["--parents", "HEAD"],
            &["--parents", "--first-parent", "HEAD"],
            &["--parents", "HEAD~3..HEAD"],
            &["--left-right", "master...side"],
            &["--left-right", "side...master"],
            &["--left-right", "--count", "master...side"],
            &["--left-right", "--count", "master"],
            &["master...side"],
            &["--abbrev-commit", "HEAD"],
            &["--abbrev-commit", "--abbrev=10", "HEAD"],
            &["--abbrev-commit", "--parents", "HEAD"],
            &["--reverse", "--parents", "HEAD"],
        ],
    );
}

#[tokio::test]
async fn rev_list_without_a_revision_is_a_usage_error() {
    let b = standard().await;
    b.same_out(&["rev-list"]);
    b.same_out(&["rev-list", "nothere"]);
    b.same_out(&["rev-list", "--count"]);
}

// ----------------------------------------------------------------------------- merge-base

#[tokio::test]
async fn merge_base_matches_git() {
    let b = standard().await;
    let c4 = b.shas["c4"].clone();
    let c3 = b.shas["c3"].clone();
    b.same_all(
        "merge-base",
        &[
            &["master", "side"],
            &["side", "master"],
            &["--all", "master", "side"],
            &["-a", "master", "side"],
            &["master", "master"],
            &["master", "v1"],
            &["v1", "master"],
            &["v2", "side"],
            &["HEAD", "HEAD~2"],
            &[c4.as_str(), c3.as_str()],
            &["master", "side", "v1"],
            &["--is-ancestor", "v1", "master"],
            &["--is-ancestor", "master", "v1"],
            &["--is-ancestor", "master", "master"],
            &["--is-ancestor", "side", "master"],
            &["master", "nope"],
            &["--is-ancestor", "nope", "master"],
        ],
    );
    b.same_out(&["merge-base"]);
    b.same_out(&["merge-base", "master"]);
    b.same_out(&["merge-base", "--is-ancestor", "master"]);
}

// ---------------------------------------------------------------------------------- describe

#[tokio::test]
async fn describe_matches_git() {
    let b = standard().await;
    let c4 = b.shas["c4"].clone();
    b.same_all(
        "describe",
        &[
            &[],
            &["--tags"],
            &["--long"],
            &["--tags", "--long"],
            &["v2"],
            &["--long", "v2"],
            &["--tags", "v1"],
            &["--tags", "--long", "v1"],
            &["--tags", "HEAD~3"],
            &["--tags", "side"],
            &["--tags", "not-a-name"],
            &["--abbrev=10"],
            &["--abbrev=0"],
            &["--abbrev=0", "--long"],
            &["--match", "v1*", "--tags"],
            &["--match=v2*"],
            &["--exclude", "v2", "--tags"],
            &["--exclude=v*", "--tags"],
            &["--always", "--tags", "HEAD~6"],
            &["--always", "HEAD~6"],
            &["HEAD~6"],
            &["--tags", "HEAD~6"],
            &[c4.as_str()],
            &["master", "side", "--tags"],
        ],
    );
}

#[tokio::test]
async fn describe_without_tags_matches_git() {
    let b = Built::new(history(), "c7").await;
    b.same_all(
        "describe",
        &[
            &[],
            &["--tags"],
            &["--always"],
            &["--always", "--abbrev=12"],
        ],
    );
    b.tag("light", "c2").await;
    b.same_all(
        "describe",
        &[&[], &["--tags"], &["--always"], &["HEAD~6", "--tags"]],
    );
}

// ------------------------------------------------------------------------------------ reflog

async fn logged() -> (Fixture, Vec<String>) {
    let f = Fixture::new().await;
    let a = f.commit(&[], &[("f.txt", b"one")]).await;
    let b = f.commit(&[a], &[("f.txt", b"two")]).await;
    let c = f.commit(&[b], &[("f.txt", b"three")]).await;
    f.set_branch("master", a).await;
    f.set_branch("master", b).await;
    f.set_branch("master", c).await;
    f.set_branch("topic", a).await;
    f.checkout_branch("master", Some(c)).await;
    let mut shas = Vec::new();
    for id in [a, b, c] {
        shas.push(f.engine.git_commit_of(id).await.unwrap().to_string());
    }
    (f, shas)
}

#[tokio::test]
async fn reflog_lists_the_branch_log_newest_first() {
    let (f, s) = logged().await;
    let short = |i: usize| s[i][..7].to_owned();
    let want = format!(
        "{} HEAD@{{0}}: fixture\n{} HEAD@{{1}}: fixture\n{} HEAD@{{2}}: fixture\n",
        short(2),
        short(1),
        short(0)
    );
    for args in [
        &["reflog"][..],
        &["reflog", "show"],
        &["reflog", "show", "HEAD"],
        &["reflog", "HEAD"],
    ] {
        let out = run(f.asc().args(args));
        assert_eq!(
            (out.code, out.stdout.as_str()),
            (0, want.as_str()),
            "{args:?}: {}",
            out.stderr
        );
    }
    let named = run(f.asc().args(["reflog", "master", "-n", "2"]));
    assert_eq!(
        named.stdout,
        format!(
            "{} master@{{0}}: fixture\n{} master@{{1}}: fixture\n",
            short(2),
            short(1)
        )
    );
    let topic = run(f.asc().args(["reflog", "show", "topic"]));
    assert_eq!(topic.stdout, format!("{} topic@{{0}}: fixture\n", short(0)));
    let full = run(f.asc().args(["reflog", "refs/heads/topic", "-1"]));
    assert_eq!(
        full.stdout,
        topic.stdout.replace("topic@", "refs/heads/topic@")
    );
}

#[tokio::test]
async fn reflog_exists_reports_through_the_exit_code() {
    let (f, _) = logged().await;
    for (args, code) in [
        (["reflog", "exists", "refs/heads/master"], 0),
        (["reflog", "exists", "master"], 0),
        (["reflog", "exists", "HEAD"], 0),
        (["reflog", "exists", "refs/heads/nothere"], 1),
    ] {
        let out = run(f.asc().args(args));
        assert_eq!(
            (out.code, out.stdout.as_str()),
            (code, ""),
            "{args:?}: {}",
            out.stderr
        );
    }
    let expire = run(f.asc().args(["reflog", "expire", "--all"]));
    assert_eq!(expire.code, 129, "{}", expire.stderr);
}
