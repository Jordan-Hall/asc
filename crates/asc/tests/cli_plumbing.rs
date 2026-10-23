//! U24e acceptance: the object and ref plumbing (`ls-files`, `ls-tree`, `cat-file`,
//! `hash-object`, `update-index`, `write-tree`, `read-tree`, `show-ref`, `for-each-ref`,
//! `symbolic-ref`, `update-ref`, `check-ref-format`, `var`) through the built binary, compared with
//! real git 2.55.
//!
//! Scenarios are built twice from one description (an asc fixture through the engine, a real git
//! repository through `fast-import`), so both sides share every commit id and the outputs are
//! compared byte for byte. The ref-filter goldens cover every atom of git's `valid_atom` table.

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
use asc_core::object::{Change, Kind, SortedMap, TagRecord};
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
    let mut c = Command::new("git"); // lint:allow-raw-git
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            c.env_remove(&key);
        }
    }
    c.current_dir(dir)
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
    /// Mismatches the comparison helpers recorded; reported together when the scenario ends.
    fails: std::cell::RefCell<Vec<String>>,
}

impl Drop for Built {
    fn drop(&mut self) {
        let fails = self.fails.borrow();
        if !fails.is_empty() && !std::thread::panicking() {
            panic!(
                "{} mismatches with git:

{}",
                fails.len(),
                fails.join(
                    "

"
                )
            );
        }
    }
}

/// Records a mismatch instead of stopping, so one run reports every difference.
macro_rules! soft_eq {
    ($built:expr, $left:expr, $right:expr, $($fmt:tt)+) => {{
        let (l, r) = (&$left, &$right);
        if l != r {
            let what = format!($($fmt)+);
            $built.fails.borrow_mut().push(format!("{what}
  asc: {l:?}
 git: {r:?}"));
        }
    }};
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

/// A path as a `fast-import` stream spells it: C-quoted when it holds control characters.
fn fast_import_path(path: &str) -> String {
    if !path.bytes().any(|b| b < 0x20 || b == b'"' || b == b'\\') {
        return path.to_owned();
    }
    let mut out = String::from("\"");
    for c in path.chars() {
        match c {
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
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
            let spelled = fast_import_path(path);
            stream.extend_from_slice(
                format!(
                    "M 100644 inline {spelled}\ndata {}\n{content}\n",
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
        // The work tree of the asc workspace holds the files of HEAD, as git's does.
        for (path, content) in &steps
            .iter()
            .find(|s| s.name == head)
            .expect("head step")
            .files
        {
            let at = f.root.join(path);
            fs::create_dir_all(at.parent().unwrap()).unwrap();
            fs::write(at, content).unwrap();
        }
        let b = Self {
            f,
            git_dir,
            ids,
            shas,
            fails: Default::default(),
        };
        b.branch("master", head).await;
        let r = b.git(&["update-ref", "-d", "refs/heads/work"]);
        assert_eq!(r.code, 0, "{}", r.stderr);
        b.f.checkout_branch("master", Some(b.ids[head])).await;
        let r = b.git(&["symbolic-ref", "HEAD", "refs/heads/master"]);
        assert_eq!(r.code, 0, "{}", r.stderr);
        let r = b.git(&["reset", "-q", "--hard", "master"]);
        assert_eq!(r.code, 0, "{}", r.stderr);
        // asc keeps no ORIG_HEAD from a `reset` the fixture did not run.
        let _ = fs::remove_file(b.git_dir.join(".git").join("ORIG_HEAD"));
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

    fn asc_env(&self, env: &[(&str, &str)], args: &[&str]) -> Out {
        let mut c = self.f.asc();
        c.envs(env.iter().copied()).args(args);
        run(&mut c)
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

    /// Sets one config key in both repositories.
    fn config(&self, key: &str, value: &str) {
        let r = self.git(&["config", key, value]);
        assert_eq!(r.code, 0, "{}", r.stderr);
        let r = self.asc(&["config", key, value]);
        assert_eq!(r.code, 0, "asc config: {}", r.stderr);
    }

    /// `asc <args>` equals `git <args>`: stdout and exit code (and stderr when git fails).
    fn same(&self, args: &[&str]) {
        let want = self.git(args);
        let got = self.asc(args);
        soft_eq!(
            self,
            got.code,
            want.code,
            "exit code of {args:?}\nasc stderr: {}\ngit stderr: {}",
            got.stderr,
            want.stderr
        );
        soft_eq!(
            self,
            got.stdout,
            want.stdout,
            "stdout of {args:?}\n--- asc stderr:\n{}",
            got.stderr
        );
        if want.code != 0 {
            let normal = |s: &str| s.replace("'git <command>", "'asc <command>");
            soft_eq!(self, got.stderr, normal(&want.stderr), "stderr of {args:?}");
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
        soft_eq!(
            self,
            got.code,
            want.code,
            "exit code of {args:?}\nasc stderr: {}\ngit stderr: {}",
            got.stderr,
            want.stderr
        );
        soft_eq!(
            self,
            got.stdout,
            want.stdout,
            "stdout of {args:?}\n--- asc stderr:\n{}",
            got.stderr
        );
    }

    /// The git sha of step `name` with `suffix` (`~1`, `^2`).
    fn sha(&self, name: &str, suffix: &str) -> String {
        format!("{}{suffix}", self.shas[name])
    }
}

const T0: i64 = 1_600_000_000;
const DAY: i64 = 86_400;

/// A history with a merge, trailers, tabs and several identities.
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
    let mut c7 = step(
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
    c7.author = Who {
        name: "No Mail",
        email: "nomail",
        time: T0 + 6 * DAY,
        tz: 0,
    };
    vec![c1, c2, c3, c4, c5, c6, c7]
}

async fn standard() -> Built {
    let b = Built::new(history(), "c7").await;
    b.branch("side", "c3").await;
    b.branch("old", "c1").await;
    b.tag("v1", "c2").await;
    b.tag("v1.10", "c4").await;
    b.tag("v1.9", "c3").await;
    b.reference("refs/remotes/origin/master", "c6").await;
    b.reference("refs/remotes/origin/HEAD", "c6").await;
    b.annotated_tag(
        "v2",
        "c4",
        "Release two\n\nWith notes.\nSigned-off-by: Rel <rel@example.org>\n",
        T0 + 3 * DAY + 60,
    )
    .await;
    b
}

// ---------------------------------------------------------------------------------- for-each-ref

/// Every atom of git's `valid_atom` table, with the argument forms git accepts.
const ATOM_FORMATS: &[&str] = &[
    "%(refname)",
    "%(refname:short)",
    "%(refname:lstrip=1)",
    "%(refname:lstrip=2)",
    "%(refname:lstrip=5)",
    "%(refname:lstrip=-1)",
    "%(refname:rstrip=1)",
    "%(refname:rstrip=2)",
    "%(refname:rstrip=-1)",
    "%(refname:strip=2)",
    "%(objecttype)",
    "%(objectsize)",
    "%(objectname)",
    "%(objectname:short)",
    "%(objectname:short=10)",
    "%(objectname:short=2)",
    "%(deltabase)",
    "%(tree)",
    "%(tree:short)",
    "%(tree:short=12)",
    "%(parent)",
    "%(parent:short)",
    "%(numparent)",
    "%(object)",
    "%(type)",
    "%(tag)",
    "%(author)",
    "%(authorname)",
    "%(authoremail)",
    "%(authoremail:trim)",
    "%(authoremail:localpart)",
    "%(authordate)",
    "%(authordate:short)",
    "%(authordate:iso)",
    "%(authordate:iso-strict)",
    "%(authordate:rfc2822)",
    "%(authordate:raw)",
    "%(authordate:unix)",
    "%(authordate:format:%Y-%m-%d %H:%M)",
    "%(committer)",
    "%(committername)",
    "%(committeremail)",
    "%(committeremail:localpart)",
    "%(committerdate)",
    "%(committerdate:iso)",
    "%(tagger)",
    "%(taggername)",
    "%(taggeremail)",
    "%(taggeremail:trim)",
    "%(taggerdate)",
    "%(taggerdate:short)",
    "%(creator)",
    "%(creatordate)",
    "%(creatordate:unix)",
    "%(subject)",
    "%(subject:sanitize)",
    "%(body)",
    "%(contents)",
    "%(contents:subject)",
    "%(contents:body)",
    "%(contents:signature)",
    "%(contents:size)",
    "%(contents:lines=1)",
    "%(contents:lines=3)",
    "%(contents:trailers)",
    "%(contents:trailers:only)",
    "%(trailers)",
    "%(trailers:only)",
    "%(trailers:only,unfold)",
    "%(trailers:key=Signed-off-by,valueonly)",
    "%(trailers:keyonly,separator=%x2C)",
    "%(signature)",
    "%(signature:grade)",
    "%(signature:signer)",
    "%(signature:key)",
    "%(signature:fingerprint)",
    "%(signature:primarykeyfingerprint)",
    "%(signature:trustlevel)",
    "%(raw)",
    "%(raw:size)",
    "%(symref)",
    "%(symref:short)",
    "%(flag)",
    "%(HEAD)",
    "%(color:red)x%(color:reset)",
    "[%(align:12)%(refname:short)%(end)]",
    "[%(align:width=12,position=right)%(refname:short)%(end)]",
    "[%(align:12,middle)%(refname:short)%(end)]",
    "[%(align:5,left)%(refname:short)%(end)]",
    "%(if)%(HEAD)%(then)current%(else)other%(end)",
    "%(if:equals=refs/heads/side)%(refname)%(then)S%(end)",
    "%(if:notequals=refs/heads/side)%(refname)%(then)N%(else)S%(end)",
    "%(if)%(subject)%(then)has-subject%(end)",
    "%(align:8)%(if)%(HEAD)%(then)*%(end)%(end)|",
    "%(describe)",
    "%(describe:tags)",
    "%(describe:tags,abbrev=4)",
    "%(describe:match=v1*)",
    "%(describe:exclude=v1*)",
    "%(ahead-behind:side)",
    "%(ahead-behind:master)",
    "literal %% and %41%0a",
];

/// Atoms that read the object a tag peels to, spelled with the `*` prefix.
const DEREF_FORMATS: &[&str] = &[
    "%(*refname)",
    "%(*refname:short)",
    "%(*objecttype)",
    "%(*objectsize)",
    "%(*objectname)",
    "%(*objectname:short)",
    "%(*deltabase)",
    "%(*tree)",
    "%(*parent)",
    "%(*numparent)",
    "%(*object)",
    "%(*type)",
    "%(*tag)",
    "%(*author)",
    "%(*authorname)",
    "%(*authoremail)",
    "%(*authordate)",
    "%(*committer)",
    "%(*committerdate:iso)",
    "%(*tagger)",
    "%(*creator)",
    "%(*creatordate)",
    "%(*subject)",
    "%(*body)",
    "%(*contents)",
    "%(*contents:lines=1)",
    "%(*trailers)",
    "%(*signature)",
    "%(*raw:size)",
    "%(*upstream)",
    "%(*HEAD)",
    "%(*symref)",
];

#[tokio::test]
async fn every_atom_matches_git() {
    let b = standard().await;
    for format in ATOM_FORMATS.iter().chain(DEREF_FORMATS) {
        let arg = format!("--format=[{format}]");
        b.same(&["for-each-ref", &arg]);
    }
}

#[tokio::test]
async fn default_format_and_selection_match_git() {
    let b = standard().await;
    b.same_all(
        "for-each-ref",
        &[
            &[],
            &["refs/heads"],
            &["refs/heads/"],
            &["refs/heads/*"],
            &["refs/tags/v*"],
            &["refs/tags/v?"],
            &["refs/tags/v1.*", "refs/heads/s*"],
            &["refs/remotes"],
            &["refs/t*"],
            &["refs/**"],
            &["refs/heads/master"],
            &["heads"],
            &["--exclude=refs/tags", "--exclude=refs/remotes"],
            &["--exclude=refs/heads/s*", "refs/heads"],
            &["--count=2"],
            &["--count=3", "--sort=-refname"],
            &["--count=0"],
            &["--ignore-case", "REFS/HEADS"],
            &["--ignore-case", "--sort=refname", "refs/Tags/V*"],
            &["--include-root-refs"],
            &[
                "--include-root-refs",
                "--format=%(refname) %(symref) [%(HEAD)] %(objectname:short)",
            ],
            &["--format=%(refname:short)", "--omit-empty"],
            &[
                "--format=%(if)%(HEAD)%(then)%(refname:short)%(end)",
                "--omit-empty",
            ],
            &["--format=%(refname:short) %(subject)", "--shell"],
            &["--format=%(refname:short) %(subject)", "--perl"],
            &["--format=%(refname:short) %(subject)", "--python"],
            &["--format=%(refname:short) %(subject)", "--tcl"],
            &["--format=%(refname:short) %(subject)", "-s"],
            &[
                "--format=%(align:12)%(refname:short)%(end)|%(subject)",
                "--shell",
            ],
            &["--format=%(if)%(HEAD)%(then)a'b%(else)c$d%(end)", "--tcl"],
            &["--format"],
        ],
    );
}

#[tokio::test]
async fn sort_keys_match_git() {
    let b = standard().await;
    let keys = [
        "refname",
        "-refname",
        "objectname",
        "-objectname",
        "objecttype",
        "objectsize",
        "-objectsize",
        "authordate",
        "-authordate",
        "committerdate",
        "-committerdate",
        "creatordate",
        "-creatordate",
        "taggerdate",
        "-taggerdate",
        "authorname",
        "committeremail",
        "subject",
        "-subject",
        "numparent",
        "-numparent",
        "version:refname",
        "-version:refname",
        "v:refname",
        "-v:refname",
        "refname:short",
        "refname:lstrip=2",
        "-*authordate",
        "*objectname",
        "authordate:iso",
        "-creatordate:short",
        "HEAD",
    ];
    for key in keys {
        b.same(&[
            "for-each-ref",
            &format!("--sort={key}"),
            "--format=%(refname) %(objectname:short)",
        ]);
    }
    b.same(&[
        "for-each-ref",
        "--sort=authorname",
        "--sort=-committerdate",
        "--format=%(refname)",
    ]);
    b.same(&[
        "for-each-ref",
        "--sort=-committerdate",
        "--sort=authorname",
        "--format=%(refname)",
    ]);
    b.same(&[
        "for-each-ref",
        "--ignore-case",
        "--sort=subject",
        "--format=%(refname) %(subject)",
    ]);
}

#[tokio::test]
async fn reachability_and_identity_filters_match_git() {
    let b = standard().await;
    let c4 = b.shas["c4"].clone();
    let c3 = b.shas["c3"].clone();
    let fmt = "--format=%(refname)";
    for args in [
        vec!["--merged=side", fmt],
        vec!["--merged", "side", fmt],
        vec!["--no-merged=side", fmt],
        vec!["--merged", fmt],
        vec!["--no-merged", fmt],
        vec!["--contains=side", fmt],
        vec!["--contains", "v1", fmt],
        vec!["--no-contains=side", fmt],
        vec!["--no-contains", "c1", fmt],
        vec!["--contains", &c3, fmt],
        vec!["--merged=v2", "--no-merged=side", fmt],
        vec!["--contains=side", "--contains=v1.10", fmt],
        vec!["--points-at=v1.10", fmt],
        vec!["--points-at", &c4, fmt],
        vec!["--points-at=master", fmt],
        vec!["--points-at=v2", fmt],
        vec!["--points-at=nonexistent", fmt],
        vec!["--merged=nonexistent", fmt],
        vec!["--contains=refs/tags/v2", "refs/tags", fmt],
    ] {
        let mut full = vec!["for-each-ref"];
        full.extend(args);
        b.same(&full);
    }
}

#[tokio::test]
async fn upstream_and_push_atoms_match_git() {
    let b = standard().await;
    b.config("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*");
    b.config("branch.master.remote", "origin");
    b.config("branch.master.merge", "refs/heads/master");
    b.config("branch.side.remote", ".");
    b.config("branch.side.merge", "refs/heads/master");
    b.config("branch.old.remote", "origin");
    b.config("branch.old.merge", "refs/heads/gone");
    let atoms = [
        "upstream",
        "upstream:short",
        "upstream:lstrip=2",
        "upstream:rstrip=1",
        "upstream:track",
        "upstream:track,nobracket",
        "upstream:trackshort",
        "upstream:remotename",
        "upstream:remoteref",
        "push",
        "push:short",
        "push:track",
        "push:trackshort",
        "push:remotename",
        "push:remoteref",
    ];
    for atom in atoms {
        b.same(&[
            "for-each-ref",
            &format!("--format=%(refname:short) [%({atom})]"),
            "refs/heads",
        ]);
    }
    b.config("push.default", "current");
    b.same(&[
        "for-each-ref",
        "--format=%(refname:short) [%(push)] [%(push:track)]",
        "refs/heads",
    ]);
    b.config("push.default", "upstream");
    b.same(&[
        "for-each-ref",
        "--format=%(refname:short) [%(push)] [%(push:track)]",
        "refs/heads",
    ]);
    b.config("remote.pushDefault", "origin");
    b.config("push.default", "nothing");
    b.same(&[
        "for-each-ref",
        "--format=%(refname:short) [%(push)] [%(push:remotename)]",
        "refs/heads",
    ]);
}

#[tokio::test]
async fn is_base_and_ahead_behind_match_git() {
    let b = standard().await;
    for target in ["side", "master", "v2", "old", "refs/remotes/origin/master"] {
        b.same(&[
            "for-each-ref",
            &format!("--format=%(refname):%(is-base:{target})"),
            "refs/heads",
        ]);
        b.same(&[
            "for-each-ref",
            &format!("--format=%(refname) %(ahead-behind:{target})"),
        ]);
    }
    b.same(&[
        "for-each-ref",
        "--format=%(refname):%(is-base:side)",
        "--sort=-refname",
    ]);
}

#[tokio::test]
async fn format_errors_match_git() {
    let b = standard().await;
    for format in [
        "%(refname:bogus)",
        "%(refname:lstrip=x)",
        "%(objectname:short=x)",
        "%(end)",
        "%(then)",
        "%(else)",
        "%(if)x",
        "%(if)x%(then)",
        "%(if)x%(end)",
        "%(if)%(then)%(then)",
        "%(if)%(then)%(else)%(else)%(end)",
        "%(if)%(then)%(end)%(end)",
        "%(align:foo)%(end)",
        "%(align:width=5,position=x)%(end)",
        "%(color:bogus)",
        "%(color)",
        "%(authordate:bogus)",
        "%(upstream:bogus)",
        "%(contents:lines=x)",
        "%(contents:bogus)",
        "%(trailers:bogus)",
        "%(signature:bogus)",
        "%(raw:foo)",
        "%(HEAD:x)",
        "%(ahead-behind)",
        "%(ahead-behind:nonexistent)",
        "%(is-base:)",
        "%(describe:bogus)",
        "%(describe:abbrev=x)",
    ] {
        b.same(&["for-each-ref", &format!("--format={format}")]);
    }
    for format in ["%(raw)"] {
        for quote in ["--shell", "--perl", "--python", "--tcl"] {
            b.same(&["for-each-ref", &format!("--format={format}"), quote]);
        }
    }
    b.same(&["for-each-ref", "--sort=bogus"]);
    b.same(&["for-each-ref", "--count=-1"]);
}

#[tokio::test]
async fn worktreepath_names_the_workspace() {
    let b = standard().await;
    let out = b.asc(&[
        "for-each-ref",
        "--format=%(refname) [%(worktreepath)]",
        "refs/heads",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let root = b.f.root.display().to_string().replace('\\', "/");
    assert!(
        out.stdout.contains(&format!("refs/heads/master [{root}]")),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains("refs/heads/side []"), "{}", out.stdout);
}

// ---------------------------------------------------------------------------------------- show-ref

#[tokio::test]
async fn show_ref_matches_git() {
    let b = standard().await;
    b.same_all(
        "show-ref",
        &[
            &[],
            &["--head"],
            &["-d"],
            &["--dereference"],
            &["--tags"],
            &["--branches"],
            &["--tags", "--branches"],
            &["--tags", "-d"],
            &["-s"],
            &["--hash"],
            &["--hash=7"],
            &["-s", "-d"],
            &["--abbrev"],
            &["--abbrev=9"],
            &["--abbrev=2"],
            &["master"],
            &["heads/side"],
            &["side", "v1"],
            &["v1"],
            &["refs/tags/v2"],
            &["-d", "v2"],
            &["origin/master"],
            &["master", "--head"],
            &["nonexistent"],
            &["--verify", "refs/heads/master"],
            &["--verify", "HEAD"],
            &["--verify", "refs/tags/v2", "-d"],
            &["--verify", "master"],
            &["--verify", "refs/heads/nonexistent"],
            &["--verify", "-q", "refs/heads/nonexistent"],
            &["--verify", "-q", "refs/heads/master"],
            &["--verify"],
            &["--verify", "refs/heads/master", "refs/heads/side"],
            &["--verify", "refs/heads/master", "refs/heads/nonexistent"],
            &["--exists", "refs/heads/master"],
            &["--exists", "refs/heads/nonexistent"],
            &["--exists", "refs/tags/v2"],
            &["--exists"],
            &["-q", "master"],
            &["-q", "nonexistent"],
            &["--", "master"],
        ],
    );
}

// ------------------------------------------------------------------------------------ check-ref-format

#[tokio::test]
async fn check_ref_format_matches_git() {
    let b = standard().await;
    for name in [
        "refs/heads/master",
        "master",
        "refs/heads/",
        "refs/heads//x",
        "/refs/heads/x",
        "refs/heads/x/",
        "refs/heads/.x",
        "refs/heads/x.lock",
        "refs/heads/x.",
        "refs/heads/a..b",
        "refs/heads/a b",
        "refs/heads/a~1",
        "refs/heads/a^",
        "refs/heads/a:b",
        "refs/heads/a?b",
        "refs/heads/a*b",
        "refs/heads/a[b",
        "refs/heads/a\\b",
        "refs/heads/a@{b",
        "refs/heads/@",
        "@",
        "refs/heads/ok-name_1",
        "refs/héads/ü",
        "a/b",
        "a/.b",
        "HEAD",
        "refs/heads/x.lock/y",
    ] {
        b.same_out(&["check-ref-format", name]);
        b.same_out(&["check-ref-format", "--allow-onelevel", name]);
        b.same_out(&["check-ref-format", "--refspec-pattern", name]);
        b.same_out(&["check-ref-format", "--normalize", name]);
        b.same_out(&["check-ref-format", "--branch", name]);
    }
    b.same_all(
        "check-ref-format",
        &[
            &["--refspec-pattern", "refs/heads/*"],
            &["--refspec-pattern", "refs/*/*"],
            &["--refspec-pattern", "refs/heads/a*b"],
            &["--normalize", "refs//heads///x"],
            &["--normalize", "/refs/heads/x"],
            &["--print", "refs//heads/x"],
            &["--no-allow-onelevel", "x"],
            &["--allow-onelevel", "--no-allow-onelevel", "x"],
            &["--branch", "master"],
            &["--branch", "HEAD"],
            &[],
            &["a", "b"],
        ],
    );
}

// ---------------------------------------------------------------------------------------------- var

#[tokio::test]
async fn var_matches_git() {
    let b = standard().await;
    let env = [
        ("GIT_AUTHOR_NAME", "Ann Author"),
        ("GIT_AUTHOR_EMAIL", "ann@example.org"),
        ("GIT_AUTHOR_DATE", "1700000000 +0200"),
        ("GIT_COMMITTER_NAME", "Cy Committer"),
        ("GIT_COMMITTER_EMAIL", "cy@example.net"),
        ("GIT_COMMITTER_DATE", "1700000100 -0530"),
        ("GIT_EDITOR", "my-editor --wait"),
        ("GIT_PAGER", "my-pager"),
        ("TERM", "xterm"),
    ];
    for var in [
        "GIT_AUTHOR_IDENT",
        "GIT_COMMITTER_IDENT",
        "GIT_EDITOR",
        "GIT_SEQUENCE_EDITOR",
        "GIT_PAGER",
        "GIT_DEFAULT_BRANCH",
    ] {
        let want = b.git_env(&env, &["var", var]);
        let got = b.asc_env(&env, &["var", var]);
        assert_eq!(
            (got.code, &got.stdout),
            (want.code, &want.stdout),
            "{var}: {}",
            got.stderr
        );
    }
    b.config("core.editor", "cfg-editor");
    b.config("core.pager", "cfg-pager");
    b.config("sequence.editor", "cfg-seq");
    b.config("init.defaultBranch", "trunk");
    b.config("user.name", "Config Name");
    b.config("user.email", "config@example.org");
    let plain = [
        ("TERM", "xterm"),
        ("GIT_AUTHOR_DATE", "1700000000 +0000"),
        ("GIT_COMMITTER_DATE", "1700000000 +0000"),
    ];
    for var in [
        "GIT_EDITOR",
        "GIT_SEQUENCE_EDITOR",
        "GIT_DEFAULT_BRANCH",
        "GIT_AUTHOR_IDENT",
        "GIT_COMMITTER_IDENT",
    ] {
        let want = b.git_env(&plain, &["var", var]);
        let got = b.asc_env(&plain, &["var", var]);
        assert_eq!(
            (got.code, &got.stdout),
            (want.code, &want.stdout),
            "{var}: {}",
            got.stderr
        );
    }
    b.same_out(&["var", "NOT_A_VARIABLE"]);
    b.same_out(&["var"]);
    let listing = b.asc_env(&plain, &["var", "-l"]);
    assert_eq!(listing.code, 0, "{}", listing.stderr);
    assert!(
        listing.stdout.contains("user.name=Config Name\n"),
        "{}",
        listing.stdout
    );
    assert!(
        listing.stdout.contains("GIT_DEFAULT_BRANCH=trunk\n"),
        "{}",
        listing.stdout
    );
    assert!(
        listing
            .stdout
            .contains("GIT_COMMITTER_IDENT=Config Name <config@example.org> 1700000000 +0000\n"),
        "{}",
        listing.stdout
    );
}

// ---------------------------------------------------------------------------- object plumbing helpers

impl Built {
    /// Writes `content` at `path` in both work trees.
    fn put(&self, path: &str, content: &str) {
        for root in [self.f.root.clone(), self.git_dir.clone()] {
            let at = root.join(path);
            fs::create_dir_all(at.parent().unwrap()).unwrap();
            fs::write(at, content).unwrap();
        }
    }

    /// Removes `path` from both work trees.
    fn unput(&self, path: &str) {
        for root in [self.f.root.clone(), self.git_dir.clone()] {
            let _ = fs::remove_file(root.join(path));
        }
    }

    fn git_in(&self, sub: &str, args: &[&str]) -> Out {
        run(git_cmd(&self.git_dir.join(sub), &self.f.sandbox.user).args(args))
    }

    fn asc_in(&self, sub: &str, args: &[&str]) -> Out {
        run(self.f.sandbox.asc(&self.f.root.join(sub)).args(args))
    }

    /// Both tools run in the directory `sub` of their work trees.
    fn same_in(&self, sub: &str, args: &[&str]) {
        let want = self.git_in(sub, args);
        let got = self.asc_in(sub, args);
        soft_eq!(
            self,
            got.code,
            want.code,
            "exit code of {args:?} in {sub}\nasc stderr: {}\ngit stderr: {}",
            got.stderr,
            want.stderr
        );
        soft_eq!(
            self,
            got.stdout,
            want.stdout,
            "stdout of {args:?} in {sub}\n--- asc stderr:\n{}",
            got.stderr
        );
    }

    /// Both tools run with `input` on standard input.
    fn same_stdin(&self, args: &[&str], input: &str) {
        let feed = |mut c: Command| {
            c.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = c.spawn().expect("spawn");
            // A command that fails before reading its input closes the pipe: Linux reports EPIPE.
            let _ = child.stdin.take().unwrap().write_all(input.as_bytes());
            let out = child.wait_with_output().unwrap();
            Out {
                code: out.status.code().unwrap_or(-1),
                stdout: text(&out.stdout),
                stderr: text(&out.stderr),
            }
        };
        let mut g = git_cmd(&self.git_dir, &self.f.sandbox.user);
        g.args(args);
        let mut c = self.f.asc();
        c.args(args);
        let (want, got) = (feed(g), feed(c));
        soft_eq!(
            self,
            got.code,
            want.code,
            "exit code of {args:?}\nasc stderr: {}\ngit stderr: {}",
            got.stderr,
            want.stderr
        );
        soft_eq!(
            self,
            got.stdout,
            want.stdout,
            "stdout of {args:?}\n--- asc stderr:\n{}",
            got.stderr
        );
        if want.code != 0 {
            soft_eq!(self, got.stderr, want.stderr, "stderr of {args:?}");
        }
    }

    /// Both indexes print the same under every listing form.
    fn same_index(&self) {
        self.same(&["ls-files", "-s"]);
        self.same(&["ls-files"]);
        self.same(&["write-tree"]);
        self.same(&["status", "--porcelain"]);
    }
}

/// A history whose last commit has names that need quoting.
async fn quoted() -> Built {
    let mut steps = history();
    let last = steps.last_mut().unwrap();
    for name in [
        "sp ace.txt",
        "ünï.txt",
        "dir with space/f.txt",
        "dot.dot..txt",
    ] {
        last.files.push((name.to_owned(), "x\n".to_owned()));
    }
    let b = Built::new(steps, "c7").await;
    b.branch("side", "c3").await;
    b.annotated_tag("v2", "c4", "Release two\n", T0 + 3 * DAY + 60)
        .await;
    b
}

// ------------------------------------------------------------------------------------------- ls-tree

#[tokio::test]
async fn ls_tree_matches_git() {
    let b = quoted().await;
    b.same_all(
        "ls-tree",
        &[
            &["HEAD"],
            &["HEAD", "sub"],
            &["HEAD", "sub/"],
            &["HEAD", "sub/b.txt"],
            &["HEAD", "a.txt", "sub/b.txt"],
            &["HEAD", "sub/nonexistent"],
            &["HEAD", "nonexistent"],
            &["HEAD", "sub", "a.txt"],
            &["HEAD", "a.txt/"],
            &["-r", "HEAD"],
            &["-r", "-t", "HEAD"],
            &["-rt", "HEAD"],
            &["-d", "HEAD"],
            &["-d", "-r", "HEAD"],
            &["-r", "HEAD", "sub"],
            &["-r", "-t", "HEAD", "sub"],
            &["-d", "HEAD", "sub"],
            &["-l", "HEAD"],
            &["-l", "-r", "HEAD"],
            &["--long", "HEAD", "sub"],
            &["--name-only", "HEAD"],
            &["--name-status", "-r", "HEAD"],
            &["--object-only", "HEAD"],
            &["--object-only", "-r", "HEAD"],
            &["--abbrev", "HEAD"],
            &["--abbrev=12", "-l", "HEAD"],
            &["--abbrev=3", "HEAD"],
            &["-z", "HEAD"],
            &["-z", "--name-only", "-r", "HEAD"],
            &["--format=%(objectmode) %(objecttype) %(objectname) %(objectsize) [%(objectsize:padded)] %(path)", "-r", "HEAD"],
            &["--format=%(path)%x00", "HEAD"],
            &["--format=%(path)%n", "HEAD"],
            &["--format=100%% %(path)", "HEAD"],
            &["-z", "--format=%(path)", "HEAD"],
            &["--format=%(path", "HEAD"],
            &["--format=%(path)", "-l", "HEAD"],
            &["--format=%(path)", "--name-only", "HEAD"],
            &["HEAD:sub"],
            &["HEAD:sub", "b.txt"],
            &["HEAD~2"],
            &["HEAD~2", "-r"],
            &["master"],
            &["side"],
            &["v2"],
            &["v2^{tree}"],
            &["HEAD^{tree}"],
            &["HEAD:"],
            &["HEAD:a.txt"],
            &["nonexistent"],
            &["HEAD:nonexistent"],
            &[],
        ],
    );
    // The same, from a subdirectory: names are relative to it.
    for args in [
        &["HEAD"][..],
        &["HEAD", "b.txt"],
        &["HEAD", "../a.txt"],
        &["--full-name", "HEAD"],
        &["--full-tree", "HEAD"],
        &["--full-tree", "HEAD", "a.txt"],
        &["-r", "HEAD"],
        &["-r", "--full-name", "HEAD"],
        &["-l", "HEAD"],
        &["HEAD", "."],
        &["-z", "HEAD"],
    ] {
        b.same_in("sub", args_with("ls-tree", args).as_slice());
    }
    b.same_in("dir with space", &["ls-tree", "HEAD"]);
}

fn args_with<'a>(command: &'a str, args: &[&'a str]) -> Vec<&'a str> {
    std::iter::once(command)
        .chain(args.iter().copied())
        .collect()
}

#[tokio::test]
async fn ls_tree_ids_are_accepted_by_the_next_command() {
    let b = standard().await;
    // The fixture never recorded tree or blob ids: printing them teaches asc.
    let listing = b.asc(&["ls-tree", "-r", "HEAD"]);
    assert_eq!(listing.code, 0, "{}", listing.stderr);
    let blob = listing
        .stdout
        .lines()
        .find(|l| l.ends_with("a.txt"))
        .unwrap()
        .split_whitespace()
        .nth(2)
        .unwrap()
        .to_owned();
    let dirs = b.asc(&["ls-tree", "HEAD"]);
    let tree = dirs
        .stdout
        .lines()
        .find(|l| l.ends_with("\tsub"))
        .unwrap()
        .split_whitespace()
        .nth(2)
        .unwrap()
        .to_owned();
    b.same(&["cat-file", "-p", &blob]);
    b.same(&["cat-file", "-t", &blob]);
    b.same(&["cat-file", "-s", &blob]);
    b.same(&["cat-file", "-p", &tree]);
    b.same(&["ls-tree", &tree]);
    b.same(&["ls-tree", "-r", &tree]);
}

// ------------------------------------------------------------------------------------------ cat-file

#[tokio::test]
async fn cat_file_matches_git() {
    let b = quoted().await;
    let c4 = b.shas["c4"].clone();
    let tree = b
        .git(&["rev-parse", "HEAD^{tree}"])
        .stdout
        .trim()
        .to_owned();
    let blob = b.git(&["rev-parse", "HEAD:a.txt"]).stdout.trim().to_owned();
    let tag = b.git(&["rev-parse", "v2"]).stdout.trim().to_owned();
    let missing = "1".repeat(40);
    for name in [
        "HEAD",
        "master",
        "side",
        "HEAD~3",
        "HEAD^{tree}",
        "HEAD:",
        "HEAD:a.txt",
        "HEAD:sub",
        "HEAD:sub/b.txt",
        "HEAD:big.txt",
        "v2",
        "v2^{}",
        "v2^{commit}",
        "v2^{tree}",
        "side^{tree}",
        &c4,
        &tree,
        &blob,
        &tag,
        "HEAD:sp ace.txt",
        "HEAD:ünï.txt",
        ":a.txt",
        ":sub/b.txt",
        ":0:a.txt",
    ] {
        for flag in ["-t", "-s", "-e", "-p"] {
            b.same(&["cat-file", flag, name]);
        }
    }
    b.same_all(
        "cat-file",
        &[
            &["blob", "HEAD:a.txt"],
            &["blob", &blob],
            &["tree", "HEAD"],
            &["tree", "v2"],
            &["tree", &tree],
            &["commit", "HEAD"],
            &["commit", "v2"],
            &["tag", "v2"],
            &["tag", "HEAD"],
            &["blob", "HEAD"],
            &["tree", "HEAD:a.txt"],
            &["commit", "HEAD:a.txt"],
            &["bogus", "HEAD"],
            &["blob", "nonexistent"],
            &["-e", "nonexistent"],
            &["-e", &missing],
            &["-t", &missing],
            &["-s", &missing],
            &["-p", &missing],
            &["blob", &missing],
            &["-t", "nonexistent"],
            &["-p", "HEAD:nonexistent"],
            &["-e", "HEAD:nonexistent"],
            &["-p"],
            &["-t"],
            &["-s"],
            &["-e"],
            &["HEAD"],
            &[],
            &["-t", "-s", "HEAD"],
            &["-p", "-e", "HEAD"],
            &["-t", "HEAD", "HEAD"],
        ],
    );
}

#[tokio::test]
async fn cat_file_batch_matches_git() {
    let b = quoted().await;
    let tree = b
        .git(&["rev-parse", "HEAD^{tree}"])
        .stdout
        .trim()
        .to_owned();
    let input = format!(
        "HEAD\nHEAD:a.txt\nnonexistent\n\n{}\nHEAD^{{tree}}\nv2\n{}\nHEAD:a.txt trailing words\nHEAD:big.txt\nside~1\n",
        b.shas["c3"],
        "1".repeat(40)
    );
    let with_tree = format!("{input}{tree}\n");
    b.same_stdin(&["cat-file", "--batch"], &with_tree);
    b.same_stdin(&["cat-file", "--batch-check"], &with_tree);
    b.same_stdin(&["cat-file", "--batch", "--buffer"], &with_tree);
    b.same_stdin(
        &["cat-file", "--batch=%(objectname) %(objecttype)"],
        &with_tree,
    );
    b.same_stdin(
        &[
            "cat-file",
            "--batch-check=%(objecttype) %(objectsize) [%(rest)]",
        ],
        &with_tree,
    );
    b.same_stdin(
        &["cat-file", "--batch-check=%(objectname) %(deltabase) 100%%"],
        &with_tree,
    );
    b.same_stdin(&["cat-file", "--batch=%(foo)"], &with_tree);
    b.same_stdin(&["cat-file", "--batch"], "");
    b.same_stdin(&["cat-file", "--batch", "-t"], "HEAD\n");
    b.same_stdin(&["cat-file", "--batch", "HEAD"], "HEAD\n");
}

// ---------------------------------------------------------------------------------------- hash-object

#[tokio::test]
async fn hash_object_matches_git() {
    let b = standard().await;
    b.put("note.txt", "plain\nlines\n");
    b.put("crlf.txt", "a\r\nb\r\n");
    b.put("crlf.bin", "a\r\nb\r\n");
    b.put("lone.txt", "a\rb\r\n");
    b.put(".gitattributes", "*.txt text\n*.bin -text\n");
    b.put("sub/deep.txt", "d\r\n");
    b.put("empty", "");
    let big = "line of text\n".repeat(200_000);
    b.put("big.dat", &big);
    b.same_all(
        "hash-object",
        &[
            &["note.txt"],
            &["crlf.txt"],
            &["crlf.bin"],
            &["lone.txt"],
            &["--no-filters", "crlf.txt"],
            &["sub/deep.txt"],
            &["empty"],
            &["big.dat"],
            &["note.txt", "crlf.txt", "empty"],
            &["-t", "blob", "note.txt"],
            &["-t", "tree", "empty"],
            &["-t", "bogus", "note.txt"],
            &["nonexistent"],
            &["--path=x.txt", "crlf.bin"],
            &["--path=x.bin", "crlf.txt"],
            &["--no-filters", "--path=x.txt", "crlf.txt"],
        ],
    );
    b.same_stdin(&["hash-object", "--stdin"], "stdin text\r\n");
    b.same_stdin(
        &["hash-object", "--stdin", "--path=any.txt"],
        "stdin text\r\n",
    );
    b.same_stdin(
        &["hash-object", "--stdin", "--path=any.bin"],
        "stdin text\r\n",
    );
    b.same_stdin(
        &["hash-object", "--stdin", "--no-filters", "--path=any.txt"],
        "stdin text\r\n",
    );
    b.same_stdin(&["hash-object", "--stdin", "note.txt"], "stdin text\r\n");
    b.same_stdin(&["hash-object", "--stdin"], "");
    // From a subdirectory.
    b.same_in("sub", &["hash-object", "deep.txt"]);
    b.same_in("sub", &["hash-object", "../note.txt"]);
}

#[tokio::test]
async fn hash_object_writes_blobs_asc_can_read_back() {
    let b = standard().await;
    b.put("fresh.txt", "fresh content\n");
    let big = "chunky line of text\n".repeat(400_000);
    b.put("big.dat", &big);
    for file in ["fresh.txt", "big.dat"] {
        let want = b.git(&["hash-object", "-w", file]);
        let got = b.asc(&["hash-object", "-w", file]);
        assert_eq!(got.stdout, want.stdout, "{}", got.stderr);
        let oid = got.stdout.trim().to_owned();
        b.same(&["cat-file", "-t", &oid]);
        b.same(&["cat-file", "-s", &oid]);
        let want = b.git(&["cat-file", "blob", &oid]);
        let got = b.asc(&["cat-file", "blob", &oid]);
        assert_eq!(got.code, 0, "{}", got.stderr);
        assert!(
            got.stdout == want.stdout,
            "blob {oid} reads back differently"
        );
    }
    b.same_stdin(&["hash-object", "-w", "--stdin"], "from stdin\n");
    let oid = b.git(&["hash-object", "--stdin"]).stdout;
    let _ = oid;
}

// ------------------------------------------------------------------------------- index plumbing

impl Built {
    /// An index file outside both work trees, one per tool.
    fn alt_index(&self) -> (String, String) {
        let tmp = self.f.sandbox.tmp.path();
        (
            tmp.join("alt-index-git").to_string_lossy().into_owned(),
            tmp.join("alt-index-asc").to_string_lossy().into_owned(),
        )
    }

    /// `asc <args>` equals `git <args>` with `GIT_INDEX_FILE` naming the alternate index.
    fn same_alt(&self, args: &[&str]) {
        let (git_file, asc_file) = self.alt_index();
        let want = self.git_env(&[("GIT_INDEX_FILE", &git_file)], args);
        let got = self.asc_env(&[("GIT_INDEX_FILE", &asc_file)], args);
        assert_eq!(
            got.code, want.code,
            "exit code of {args:?} on the alternate index\nasc stderr: {}\ngit stderr: {}",
            got.stderr, want.stderr
        );
        soft_eq!(
            self,
            got.stdout,
            want.stdout,
            "stdout of {args:?} on the alternate index\n--- asc stderr:\n{}",
            got.stderr
        );
        if want.code != 0 {
            soft_eq!(
                self,
                got.stderr,
                want.stderr,
                "stderr of {args:?} on the alternate index"
            );
        }
    }

    /// The blob `content` hashes to, written in both object stores.
    fn blob(&self, content: &str) -> String {
        let hold = self.f.sandbox.tmp.path().join("blob-source.tmp");
        fs::write(&hold, content).unwrap();
        let path = hold.to_string_lossy().into_owned();
        let want = self.git(&["hash-object", "-w", &path]);
        let got = self.asc(&["hash-object", "-w", &path]);
        assert_eq!(want.code, 0, "{}", want.stderr);
        assert_eq!(got.code, 0, "{}", got.stderr);
        assert_eq!(got.stdout, want.stdout);
        want.stdout.trim().to_owned()
    }
}

#[tokio::test]
async fn ls_files_matches_git() {
    let b = quoted().await;
    b.same_all(
        "ls-files",
        &[
            &[],
            &["-c"],
            &["--cached"],
            &["-s"],
            &["--stage"],
            &["-z"],
            &["-s", "-z"],
            &["sub"],
            &["sub/"],
            &["-s", "sub"],
            &["a.txt", "nonexistent"],
            &["nonexistent"],
            &["--error-unmatch", "nonexistent"],
            &["--error-unmatch", "a.txt", "nonexistent"],
            &["--error-unmatch", "a.txt"],
            &["--full-name"],
            &["*.txt"],
            &["sub/*.txt"],
            &["-s", "sp ace.txt"],
            &["ünï.txt"],
        ],
    );
    for args in [
        &[][..],
        &["-s"],
        &[".."],
        &["../a.txt"],
        &["--full-name"],
        &["--full-name", ".."],
        &["-z"],
        &["nonexistent"],
    ] {
        b.same_in("sub", args_with("ls-files", args).as_slice());
    }
    b.same_in("dir with space", &["ls-files"]);

    // Untracked files, with and without the standard exclusions.
    b.put(".gitignore", "*.log\nbuild/\n");
    b.put("x.log", "log\n");
    b.put("build/out.o", "o\n");
    b.put("keep/y.txt", "y\n");
    b.put("sub/u2.txt", "u2\n");
    b.put("top.txt", "top\n");
    b.same_all(
        "ls-files",
        &[
            &["-o"],
            &["--others"],
            &["-o", "--exclude-standard"],
            &["-o", "-z"],
            &["-o", "sub"],
            &["-o", "--exclude-standard", "sub"],
            &["-o", "keep"],
            &["-o", "top.txt", "x.log"],
            &["-o", "-c"],
            &["-c", "-o"],
            &["-s", "-o"],
            &["-o", "--full-name"],
        ],
    );
    b.same_in("sub", &["ls-files", "-o"]);
    b.same_in("sub", &["ls-files", "-o", "--exclude-standard"]);
    b.same_in("sub", &["ls-files", "-o", "--full-name"]);
}

#[tokio::test]
async fn update_index_matches_git() {
    let b = standard().await;
    b.put("new.txt", "new\n");
    b.put("sub/deeper.txt", "deeper\n");
    b.same(&["update-index", "new.txt"]);
    b.same_index();
    b.same(&["update-index", "--add", "new.txt"]);
    b.same_index();
    b.same(&["update-index", "--add", "sub/deeper.txt"]);
    b.same_index();
    // A modified tracked file.
    b.put("a.txt", "changed\n");
    b.same(&["status", "--porcelain"]);
    b.same(&["update-index", "a.txt"]);
    b.same_index();
    // A deleted file needs --remove; --force-remove works with the file present.
    b.unput("d.txt");
    b.same(&["update-index", "d.txt"]);
    b.same(&["update-index", "--remove", "d.txt"]);
    b.same_index();
    b.same(&["update-index", "--force-remove", "big.txt"]);
    b.same_index();
    b.same(&["update-index", "--remove", "never-there.txt"]);
    b.same(&["update-index", "--force-remove", "never-there.txt"]);
    // Entries by id.
    let blob = b.blob("cached\n");
    b.same(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("100644,{blob},cached.txt"),
    ]);
    b.same(&[
        "update-index",
        "--add",
        "--cacheinfo",
        "100755",
        &blob,
        "exe.sh",
    ]);
    b.same(&[
        "update-index",
        "--add",
        "--cacheinfo",
        "120000",
        &blob,
        "link",
    ]);
    b.same_index();
    b.same(&[
        "update-index",
        "--cacheinfo",
        &format!("100644,{blob},sub/b.txt"),
    ]);
    b.same_index();
    // asc stages only content it can read: an id it has never seen is refused where git accepts it.
    let ghost = b.asc(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("100644,{},ghost.txt", "1".repeat(40)),
    ]);
    assert_eq!(ghost.code, 128, "{}", ghost.stderr);
    assert!(ghost.stderr.contains("invalid object"), "{}", ghost.stderr);
    b.same(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("100644,{blob}"),
    ]);
    b.same(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("100644,{blob},../escape.txt"),
    ]);
    b.same(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("100644,{blob},.git/config"),
    ]);
    b.same(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("100644,{blob},/abs.txt"),
    ]);
    b.same_index();
    // Errors.
    b.same(&["update-index", "never-there.txt"]);
    b.same(&["update-index", "--add", "never-there.txt"]);
    b.same_out(&["update-index", "../outside.txt"]);
    // --refresh lists the files whose content differs.
    b.put("sub/b.txt", "edited behind the index\n");
    b.same(&["update-index", "--refresh"]);
    b.same(&["update-index", "-q", "--refresh"]);
    b.same(&["update-index", "--really-refresh"]);
    b.same_index();
    // From a subdirectory.
    b.put("sub/more.txt", "more\n");
    b.same_in("sub", &["update-index", "--add", "more.txt"]);
    b.same_in("sub", &["update-index", "--add", "../new2.txt"]);
    b.put("new2.txt", "n2\n");
    b.same_in("sub", &["update-index", "--add", "../new2.txt"]);
    b.same_index();
    b.same_in("sub", &["update-index", "--force-remove", "more.txt"]);
    b.same_index();
}

#[tokio::test]
async fn write_tree_and_read_tree_match_git() {
    let b = standard().await;
    b.same(&["write-tree"]);
    b.same(&["write-tree", "--missing-ok"]);
    b.same(&["write-tree", "--prefix=sub/"]);
    b.same(&["write-tree", "--prefix=nonexistent/"]);
    // The tree written for a changed index reads back identically.
    b.put("new.txt", "new\n");
    b.put("sub/deeper/x.txt", "x\n");
    b.same(&["update-index", "--add", "new.txt", "sub/deeper/x.txt"]);
    b.same(&["write-tree"]);
    let tree = b.git(&["write-tree"]).stdout.trim().to_owned();
    b.same(&["cat-file", "-p", &tree]);
    b.same(&["cat-file", "-t", &tree]);
    b.same(&["ls-tree", "-r", &tree]);
    b.same(&["write-tree", "--prefix=sub/"]);
    b.same(&["write-tree", "--prefix=sub"]);
    // A tree with a hole in it cannot be staged in the first place (see update_index_matches_git).
    b.same(&["write-tree", "--missing-ok"]);

    // read-tree replaces the index and leaves the work tree alone.
    b.same(&["read-tree", "HEAD~1"]);
    b.same_index();
    b.same(&["read-tree", "HEAD"]);
    b.same_index();
    b.same(&["read-tree", "v2"]);
    b.same_index();
    let head_tree = b
        .git(&["rev-parse", "HEAD^{tree}"])
        .stdout
        .trim()
        .to_owned();
    b.same(&["read-tree", &head_tree]);
    b.same_index();
    b.same(&["read-tree", "--empty"]);
    b.same_index();
    b.same(&["read-tree", "HEAD"]);
    b.same(&["read-tree", "-i", "old"]);
    b.same_index();
    b.same(&["read-tree", "--prefix=vendor/", "side"]);
    b.same_index();
    b.same(&["write-tree"]);
    b.same(&["read-tree", "--prefix=vendor/", "side"]);
    b.same(&["read-tree", "--prefix=vendor/sub/", "old"]);
    b.same(&["read-tree", "--prefix=../x/", "old"]);
    b.same(&["read-tree", "nonexistent"]);
    b.same(&["read-tree", "HEAD:a.txt"]);
    b.same(&["read-tree"]);
    b.same(&["read-tree", "HEAD"]);
    b.same_index();
    b.same(&["read-tree", "--empty", "HEAD"]);
    b.same(&["read-tree"]);
    b.same_index();
}

#[tokio::test]
async fn index_plumbing_follows_git_index_file() {
    let b = standard().await;
    // An index file that does not exist yet starts empty; the default index is untouched.
    b.same_alt(&["ls-files", "-s"]);
    b.same_alt(&["read-tree", "HEAD~2"]);
    b.same_alt(&["ls-files", "-s"]);
    b.same_alt(&["write-tree"]);
    b.same_index();
    b.put("only-alt.txt", "alt\n");
    b.same_alt(&["update-index", "--add", "only-alt.txt"]);
    b.same_alt(&["ls-files", "-s"]);
    b.same_alt(&["write-tree"]);
    let blob = b.blob("alt cached\n");
    b.same_alt(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("100644,{blob},alt/cached.txt"),
    ]);
    b.same_alt(&["ls-files", "-s"]);
    b.same_alt(&["update-index", "--force-remove", "a.txt"]);
    b.same_alt(&["ls-files", "-s"]);
    b.same_alt(&["read-tree", "--empty"]);
    b.same_alt(&["ls-files", "-s"]);
    b.same_alt(&["write-tree"]);
    b.same_alt(&["read-tree", "HEAD"]);
    b.same_alt(&["ls-files", "-s"]);
    // Nothing above touched the default index.
    b.same_index();
    b.same(&["status", "--porcelain"]);
}

// ------------------------------------------------------------------------- update-ref, symbolic-ref

#[tokio::test]
async fn update_ref_keeps_the_staged_state() {
    let b = standard().await;
    b.put("a.txt", "staged edit\n");
    b.put("new.txt", "new\n");
    b.same(&["update-index", "a.txt"]);
    b.same(&["update-index", "--add", "new.txt"]);
    b.put("sub/b.txt", "worktree only\n");
    b.same(&["status", "--porcelain"]);
    b.same_index();

    // Moving the branch HEAD names keeps the staged state.
    b.same(&["update-ref", "refs/heads/master", "HEAD~1"]);
    b.same(&["rev-parse", "HEAD"]);
    b.same(&["status", "--porcelain"]);
    b.same(&["status", "--porcelain=v2", "--branch"]);
    b.same_index();
    // By HEAD, which resolves to the branch.
    b.same(&["update-ref", "HEAD", "master~1"]);
    b.same(&["rev-parse", "HEAD"]);
    b.same(&["status", "--porcelain"]);
    b.same_index();
    b.same(&["update-ref", "-m", "move it", "refs/heads/master", "c5"]);
    b.same(&["status", "--porcelain"]);

    // Compare-and-swap.
    let c4 = b.sha("c4", "");
    let c6 = b.sha("c6", "");
    let zero = "0".repeat(40);
    b.same(&["update-ref", "refs/heads/master", &c6, &c4]);
    b.same(&["update-ref", "refs/heads/master", &c6, &c6]);
    b.same(&["update-ref", "refs/heads/fresh", &c6, &zero]);
    b.same(&["update-ref", "refs/heads/fresh", &c4, &zero]);
    b.same(&["update-ref", "refs/heads/fresh", &c4, &c6]);
    b.same(&["update-ref", "refs/heads/never", &c4, &c6]);
    b.same(&["update-ref", "refs/heads/fresh2", &c4]);
    b.same(&["update-ref", "refs/heads/fresh3", "fresh2"]);
    b.same(&["update-ref", "refs/tags/t-new", "v2"]);
    b.same(&["update-ref", "refs/tags/t-new2", "c2"]);
    b.same(&["update-ref", "refs/heads/from-tag", "v2"]);
    b.same(&["update-ref", "refs/heads/bogus", "nonexistent"]);
    b.same(&["update-ref", "refs/heads/bogus", &"3".repeat(40)]);
    b.same(&["update-ref", "refs/heads/a..b", &c4]);
    b.same(&["update-ref", "refs/heads/x"]);
    b.same(&["update-ref"]);
    b.same(&["for-each-ref", "--format=%(refname) %(objectname)"]);
    b.same(&["show-ref"]);

    // Deleting.
    b.same(&["update-ref", "-d", "refs/heads/fresh3"]);
    b.same(&["update-ref", "-d", "refs/heads/fresh2", &c6]);
    b.same(&["update-ref", "-d", "refs/heads/fresh2", &c4]);
    b.same(&["update-ref", "-d", "refs/heads/fresh2"]);
    b.same(&["update-ref", "-d", "refs/heads/never"]);
    b.same(&["update-ref", "-d", "refs/tags/t-new"]);
    b.same(&["update-ref", "-d"]);
    b.same(&["update-ref", "refs/heads/fresh", &zero]);
    b.same(&["show-ref"]);
    b.same(&["status", "--porcelain"]);
    b.same_index();
}

#[tokio::test]
async fn symbolic_ref_keeps_the_staged_state() {
    let b = standard().await;
    b.put("a.txt", "staged edit\n");
    b.same(&["update-index", "a.txt"]);
    b.same(&["status", "--porcelain"]);

    b.same(&["symbolic-ref", "HEAD"]);
    b.same(&["symbolic-ref", "--short", "HEAD"]);
    b.same(&["symbolic-ref", "-q", "HEAD"]);
    b.same(&["symbolic-ref", "refs/remotes/origin/HEAD"]);
    b.same(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]);
    b.same(&["symbolic-ref", "refs/heads/master"]);
    b.same(&["symbolic-ref", "-q", "refs/heads/master"]);
    b.same(&["symbolic-ref", "nonexistent"]);
    b.same(&["symbolic-ref"]);

    // HEAD to another branch: the stage keeps its content, status compares it with the new HEAD.
    b.same(&["symbolic-ref", "HEAD", "refs/heads/side"]);
    b.same(&["symbolic-ref", "HEAD"]);
    b.same(&["rev-parse", "HEAD"]);
    b.same(&["status", "--porcelain"]);
    b.same(&["status", "--porcelain=v2", "--branch"]);
    b.same_index();
    // To a branch that does not exist: an unborn HEAD.
    b.same(&["symbolic-ref", "HEAD", "refs/heads/other"]);
    b.same(&["symbolic-ref", "HEAD"]);
    b.same(&["symbolic-ref", "--short", "HEAD"]);
    b.same(&["rev-parse", "--verify", "-q", "HEAD"]);
    b.same(&["status", "--porcelain"]);
    b.same_index();
    b.same(&["symbolic-ref", "HEAD", "refs/heads/master"]);
    b.same(&["status", "--porcelain"]);
    b.same_index();
    // Errors.
    b.same(&["symbolic-ref", "HEAD", "master"]);
    b.same(&["symbolic-ref", "HEAD", "refs/heads/a..b"]);
    b.same(&["symbolic-ref", "HEAD", "refs/tags/v1", "extra"]);
    b.same(&["symbolic-ref", "-m", "switch", "HEAD", "refs/heads/side"]);
    b.same(&["status", "--porcelain"]);
    b.same_index();
}
