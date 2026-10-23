//! U24a acceptance: `log` and `show` through the built binary, compared with real git 2.55.
//!
//! Every scenario is built twice from one description: in an asc fixture (engine commits, so the
//! git commit ids derive from the same trees, identities and messages) and in a real git
//! repository (one `fast-import` stream). The two sides then agree on every commit id and the
//! outputs are compared byte for byte.

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
    let mut c = Command::new("git"); // lint:allow-raw-git
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
        } else {
            stream.extend_from_slice(b"from 0000000000000000000000000000000000000000\n");
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
    let gone = run(git_cmd(dir, home).args(["update-ref", "-d", "refs/heads/work"]));
    assert_eq!(gone.code, 0, "{}", gone.stderr);
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
        let shape = if args.contains(&"--graph") {
            self.git(&["log", "--all", "--format=%h %p|%at"]).stdout
        } else {
            String::new()
        };
        assert_eq!(
            got.stdout, want.stdout,
            "stdout of {args:?}\n--- asc stderr:\n{}\nSHAPE\n{shape}",
            got.stderr
        );
        if want.code != 0 {
            let normal = |s: &str| s.replace("'git <command>", "'asc <command>");
            assert_eq!(got.stderr, normal(&want.stderr), "stderr of {args:?}");
        }
    }

    /// `same` for `log` with each argument list.
    fn same_log(&self, lists: &[&[&str]]) {
        for list in lists {
            let args: Vec<&str> = std::iter::once("log").chain(list.iter().copied()).collect();
            self.same(&args);
        }
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

// ---------------------------------------------------------------------------------- ids

#[tokio::test]
async fn ids_are_git_sha1_and_oneline_equals_git() {
    let b = standard().await;
    let got = b.asc(&["log", "--format=%H"]);
    assert_eq!(got.code, 0, "{}", got.stderr);
    let lines: Vec<&str> = got.stdout.lines().collect();
    assert_eq!(lines.len(), 7);
    assert!(
        lines
            .iter()
            .all(|l| l.len() == 40 && l.bytes().all(|c| c.is_ascii_hexdigit())),
        "{lines:?}"
    );
    b.same_log(&[&["--format=%H"], &["--oneline"], &[]]);
}

#[tokio::test]
async fn full_git_ids_name_commits() {
    let b = standard().await;
    let sha = b.shas["c4"].clone();
    let parent = format!("{sha}~1");
    let not = format!("^{sha}");
    b.same_log(&[
        &[sha.as_str(), "--oneline"],
        &[parent.as_str(), "--oneline"],
        &[not.as_str(), "master", "--oneline"],
    ]);
}

// ----------------------------------------------------------------------------- pretty formats

#[tokio::test]
async fn builtin_pretty_formats_match_git() {
    let b = standard().await;
    for fmt in [
        "oneline",
        "short",
        "medium",
        "full",
        "fuller",
        "raw",
        "email",
        "mboxrd",
        "reference",
    ] {
        b.same_log(&[
            &[&format!("--pretty={fmt}")],
            &["-3", &format!("--pretty={fmt}"), "--abbrev-commit"],
        ]);
    }
    b.same_log(&[
        &["--abbrev=12", "--abbrev-commit", "--oneline"],
        &["--no-abbrev-commit"],
        &["--no-abbrev", "--oneline"],
    ]);
}

#[tokio::test]
async fn every_placeholder_matches_git() {
    let b = standard().await;
    std::fs::write(b.f.root.join(".mailmap"), "Robert B <rob@new.example> <bob@example.com>\nCy Mapped <cy@mapped.example> Cy Committer <cy@example.net>\n").unwrap();
    std::fs::write(b.git_dir.join(".mailmap"), "Robert B <rob@new.example> <bob@example.com>\nCy Mapped <cy@mapped.example> Cy Committer <cy@example.net>\n").unwrap();
    let placeholders = [
        "%H",
        "%h",
        "%T",
        "%t",
        "%P",
        "%p",
        "%an",
        "%aN",
        "%ae",
        "%aE",
        "%al",
        "%aL",
        "%ad",
        "%aD",
        "%ar",
        "%at",
        "%ai",
        "%aI",
        "%as",
        "%ah",
        "%cn",
        "%cN",
        "%ce",
        "%cE",
        "%cl",
        "%cL",
        "%cd",
        "%cD",
        "%cr",
        "%ct",
        "%ci",
        "%cI",
        "%cs",
        "%ch",
        "%d",
        "%D",
        "%S",
        "%e",
        "%s",
        "%f",
        "%b",
        "%B",
        "%N",
        "%GG",
        "%G?",
        "%GS",
        "%GK",
        "%GF",
        "%GP",
        "%GT",
        "%gD",
        "%gd",
        "%gn",
        "%gN",
        "%ge",
        "%gE",
        "%gs",
        "%m",
        "%n",
        "%%",
        "%x00",
        "%x41%x4a",
        "%(count)",
        "%(total)",
        "%(trailers)",
        "%(trailers:only)",
        "%(trailers:key=Signed-off-by)",
        "%(trailers:key=Bug,valueonly)",
        "%(trailers:keyonly,separator=%x2C )",
        "%(trailers:unfold,only=false)",
        "%(trailers:key_value_separator=%x3D,only)",
        "%(decorate)",
        "%(decorate:prefix=<,suffix=>,separator=;,pointer=@,tag=T:)",
        "%(decorate:prefix=,suffix=)",
        "%(describe)",
        "%(describe:tags)",
        "%(describe:abbrev=10)",
        "%(describe:tags,match=v*)",
        "%(describe:exclude=v*)",
        "%Cred",
        "%Cgreen",
        "%Cblue",
        "%Creset",
        "%C(bold red)",
        "%C(auto)",
        "%C(always,blue)",
        "%C(never,red)",
        "%C(reset)%s",
        "%+s",
        "%-s",
        "% s",
        "%+b",
        "%-b",
        "% b",
        "%+d",
        "%-d",
        "% d",
        "a%-b|%n%n%-s",
        "%<(12)%an|",
        "%<(12,trunc)%an|",
        "%<(6,ltrunc)%an|",
        "%<(7,mtrunc)%an|",
        "%>(12)%an|",
        "%>(6)%s|",
        "%><(30)%an|",
        "%<|(30)%an|",
        "%>|(30)%an|",
        "%>>|(14)%s|",
        "[    %>>(6)%an]",
        "%<(5)%<(8)%an|",
        "%<(10)%Cred%an%Creset|",
        "%w(20,2,4)%B",
        "%w(0,0,0)%s",
        "%w(30,1,3)%s %b|%w(0)%an",
        "%w(10)%an and others",
        "%w(,1,2)%an",
        "%<(0)%an",
        "%<(x)%an",
        "%w(8,x)%an",
        "50% done",
        "%<(10 )%an|",
    ];
    for fmt in placeholders {
        for style in ["--format=", "--pretty=format:", "--pretty=tformat:"] {
            let arg = format!("{style}{fmt}");
            b.same_log(&[&[&arg]]);
        }
    }
}

#[tokio::test]
async fn placeholders_with_dates_options_and_color() {
    let b = standard().await;
    for date in [
        "default",
        "iso",
        "iso-strict",
        "rfc",
        "short",
        "raw",
        "unix",
        "relative",
        "human",
        "format:%Y/%m/%d %H:%M:%S %a %b %j %z",
        "iso8601",
        "local",
        "format-local:%H:%M",
    ] {
        let arg = format!("--date={date}");
        b.same_log(&[
            &[&arg],
            &[&arg, "--format=%ad|%cd"],
            &[&arg, "--pretty=fuller"],
        ]);
    }
    b.same_log(&[
        &["--relative-date"],
        &["--color=always", "--format=%C(yellow)%h%Creset %s"],
        &["--color=always", "--decorate", "--oneline"],
        &["--color=always"],
    ]);
}

#[tokio::test]
async fn padding_uses_display_width() {
    let steps = vec![
        {
            let mut s = step(
                "w1",
                &[],
                &[("a", "x\n")],
                T0,
                "Wide: \u{65e5}\u{672c}\u{8a9e} text\n",
            );
            s.author.name = "\u{5ddd}\u{5d0e}";
            s.committer = s.author;
            s
        },
        step(
            "w2",
            &["w1"],
            &[("a", "y\n")],
            T0 + 10,
            "Combining accent\n",
        ),
    ];
    let b = Built::new(steps, "w2").await;
    b.same_log(&[
        &["--format=%<(10)%s|"],
        &["--format=%>(12)%an|"],
        &["--format=%<(8,ltrunc)%s|%<(6,trunc)%an|"],
        &["--format=%><(14)%s|"],
    ]);
}

// -------------------------------------------------------------------------- selection and order

#[tokio::test]
async fn limiting_ranges_and_order_match_git() {
    let b = standard().await;
    b.same_log(&[
        &["-n", "3", "--oneline"],
        &["-2", "--oneline"],
        &["--max-count=4", "--skip=1", "--oneline"],
        &["--skip=5", "--oneline"],
        &["-n", "0"],
        &["--reverse", "--oneline"],
        &["--reverse", "-3", "--oneline"],
        &["v1..master", "--oneline"],
        &["master..v1", "--oneline"],
        &["^v1", "master", "--oneline"],
        &["master", "--not", "v1", "--oneline"],
        &["side...master", "--oneline", "--left-right"],
        &["side...master", "--oneline"],
        &["master...side", "--format=%m %h %s", "--left-right"],
        &["side", "v2", "--oneline"],
        &["--all", "--oneline"],
        &["--branches", "--oneline"],
        &["--tags", "--oneline"],
        &["--remotes", "--oneline"],
        &["--first-parent", "--oneline"],
        &["--merges", "--oneline"],
        &["--no-merges", "--oneline"],
        &["--min-parents=2", "--oneline"],
        &["--max-parents=0", "--oneline"],
        &["--topo-order", "--oneline"],
        &["--date-order", "--oneline"],
        &["--author-date-order", "--oneline"],
        &["HEAD~3", "--oneline"],
        &["HEAD^2", "--oneline"],
        &["master^!", "--oneline"],
        &["c5^@", "--oneline"],
    ]);
}

#[tokio::test]
async fn filters_match_git() {
    let b = standard().await;
    b.same_log(&[
        &["--author=Bob", "--oneline"],
        &["--author=bob@example.com", "--oneline"],
        &["--author", "Ann", "--author", "Side", "--oneline"],
        &["--committer=Cy", "--oneline"],
        &["--grep=work", "--oneline"],
        &["--grep=WORK", "-i", "--oneline"],
        &["--grep", "Merge", "--grep", "Rename", "--oneline"],
        &["--grep=^Third", "--oneline"],
        &["-E", "--grep=Main|Side", "--oneline"],
        &["-F", "--grep=Main work", "--oneline"],
        &["--grep=a\\(b\\|d\\)", "--oneline"],
        &["--grep=work", "--invert-grep", "--oneline"],
        &["--since=2020-09-15 00:00:00", "--oneline"],
        &["--until=2020-09-15 00:00:00", "--oneline"],
        &[
            "--after=2020-09-14 00:00:00",
            "--before=2020-09-18 00:00:00",
            "--oneline",
        ],
        &[
            "--since=2020-09-15 00:00:00",
            "--until=2020-09-18 00:00:00",
            "--format=%ad",
        ],
        &["--author=nobody"],
    ]);
}

#[tokio::test]
async fn path_limits_match_git() {
    let b = standard().await;
    b.same_log(&[
        &["--oneline", "--", "a.txt"],
        &["--oneline", "--", "c.txt"],
        &["--oneline", "--", "d.txt"],
        &["--oneline", "--", "big.txt", "d.txt"],
        &["--oneline", "--", "no/such"],
        &["--oneline", "c4", "--", "a.txt"],
        &["-1", "--oneline", "--", "a.txt"],
        &["--oneline", "--follow", "--", "a.txt"],
        &["-p", "--oneline", "--", "a.txt"],
        &["--stat", "--oneline", "--", "a.txt"],
    ]);
    // A path that exists but is no revision does not need `--`.
    b.same(&["log", "--oneline", "a.txt"]);
}

// ------------------------------------------------------------------------------------- graph

#[tokio::test]
async fn graph_matches_git_on_the_merge_history() {
    let b = standard().await;
    b.same_log(&[
        &["--graph", "--oneline"],
        &["--graph"],
        &["--graph", "--pretty=full"],
        &["--graph", "--format=%h %s%n%b"],
        &["--graph", "--pretty=format:%h"],
        &["--graph", "--all", "--oneline", "--decorate"],
        &["--graph", "--oneline", "-3"],
        &["--graph", "--oneline", "side...master", "--left-right"],
        &["--graph", "--oneline", "--first-parent"],
        &["--graph", "--oneline", "v1..master"],
        &["--graph", "--stat", "--oneline"],
        &["--graph", "-p", "--pretty=short", "-4"],
        &["--graph", "--date-order", "--oneline"],
        &["--graph", "--color=always", "--oneline"],
    ]);
    let r = b.asc(&["log", "--graph", "--reverse"]);
    assert_eq!(r.code, 128);
    assert_eq!(
        r.stderr,
        "fatal: options '--reverse' and '--graph' cannot be used together\n"
    );
}

/// A random history: some roots, merges of two or three parents, equal and skewed timestamps.
fn random_history(seed: u64, n: usize, skew: bool) -> Vec<Step> {
    let mut state = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let mut next = move |bound: u64| {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) % bound
    };
    let mut steps: Vec<Step> = Vec::new();
    for i in 0..n {
        let mut parents: Vec<String> = Vec::new();
        if i > 0 && next(100) >= 4 {
            let pick = |next: &mut dyn FnMut(u64) -> u64| {
                if next(100) < 55 {
                    i - 1
                } else {
                    next(i as u64) as usize
                }
            };
            parents.push(format!("n{}", pick(&mut next)));
            if next(100) < 28 {
                let second = format!("n{}", pick(&mut next));
                if !parents.contains(&second) {
                    parents.push(second);
                }
                if next(100) < 18 {
                    let third = format!("n{}", pick(&mut next));
                    if !parents.contains(&third) {
                        parents.push(third);
                    }
                }
            }
        }
        let tick = if next(100) < 15 { 0 } else { 100 };
        let jitter = if skew { next(250) as i64 - 120 } else { 0 };
        let time = T0 + steps.len() as i64 * 100 + tick + jitter;
        let body = format!("file {i}\n");
        let mut s = step(
            &format!("n{i}"),
            &[],
            &[("f.txt", &body)],
            time.max(1),
            &format!("commit {i}\n"),
        );
        s.parents = parents;
        steps.push(s);
    }
    steps
}

async fn random_scenario(seed: u64, n: usize, skew: bool) -> Built {
    let steps = random_history(seed, n, skew);
    let head = format!("n{}", n - 1);
    let b = Built::new(steps, &head).await;
    for i in 0..n {
        b.tag(&format!("t{i}"), &format!("n{i}")).await;
    }
    b
}

#[tokio::test]
async fn graph_and_order_match_git_on_random_histories() {
    for seed in 1..=14u64 {
        let b = random_scenario(seed, 22, seed % 3 == 0).await;
        b.same_log(&[
            &["--oneline"],
            &["--graph", "--oneline"],
            &["--topo-order", "--oneline"],
            &["--date-order", "--oneline"],
            &["--all", "--graph", "--oneline"],
            &["--graph", "--oneline", "-9"],
            &["--oneline", "t5..master"],
            &["--graph", "--oneline", "t5..master"],
            &["--graph", "--oneline", "master", "^t9", "^t3"],
            &["--oneline", "t7...t15", "--left-right"],
            &["--graph", "--oneline", "--first-parent"],
            &["--oneline", "--reverse", "--topo-order"],
        ]);
        if seed <= 3 {
            b.same_log(&[
                &["--graph"],
                &["--graph", "--format=%h|%P%n%s"],
                &["--graph", "--pretty=fuller", "-6"],
            ]);
        }
    }
}

// -------------------------------------------------------------------------------- decorations

#[tokio::test]
async fn decorations_match_git() {
    let b = standard().await;
    b.branch("zeta", "c6").await;
    b.tag("t-lightweight", "c6").await;
    b.reference("refs/stash", "c5").await;
    b.reference("refs/foo/bar", "c5").await;
    b.reference("refs/remotes/origin/HEAD", "c6").await;
    b.same_log(&[
        &["--decorate", "--oneline"],
        &["--decorate=full", "--oneline"],
        &["--decorate=short"],
        &["--no-decorate", "--oneline"],
        &["--oneline"],
        &["--decorate", "--format=%h %d"],
        &["--format=%h %D"],
        &["--decorate=full", "--format=%d"],
        &["--decorate", "--source", "--oneline", "master", "side"],
        &["--source", "--format=%h %S", "master", "side"],
    ]);
    // A detached HEAD.
    let sha = b.shas["c6"].clone();
    b.git(&["update-ref", "--no-deref", "HEAD", &sha]);
    let proj = asc_git::projection::Projection::open(&b.f.root).unwrap();
    proj.write_head(&asc_git::projection::Head::Detached(
        b.f.engine.git_commit_of(b.ids["c6"]).await.unwrap(),
    ))
    .unwrap();
    asc_state::state::workspaces::update_head_base(&b.f.db, b.f.ws, "HEAD", Some(b.ids["c6"]))
        .await
        .unwrap();
    b.same_log(&[&["--decorate", "--oneline", "-3"]]);
}

// -------------------------------------------------------------------------------------- diffs

#[tokio::test]
async fn diffs_under_the_message_match_git() {
    let b = standard().await;
    for fmt in ["", "--oneline", "--pretty=full", "--format=%h"] {
        let mut base: Vec<&str> = Vec::new();
        if !fmt.is_empty() {
            base.push(fmt);
        }
        for extra in [
            &["-p"][..],
            &["-p", "-U1"],
            &["--stat"],
            &["--stat", "-p"],
            &["--stat=40,20"],
            &["--shortstat"],
            &["--numstat"],
            &["--name-only"],
            &["--name-status"],
            &["--raw"],
            &["--summary"],
            &["--stat", "--summary"],
            &["--name-status", "--no-renames"],
            &["-p", "--no-renames"],
            &["-s"],
            &["-p", "--patience"],
            &["-p", "--minimal"],
            &["--first-parent", "-p"],
            &["--stat", "-z"],
        ] {
            let mut args = base.clone();
            args.extend_from_slice(extra);
            b.same_log(&[&args]);
        }
    }
}

#[tokio::test]
async fn show_matches_git() {
    let b = standard().await;
    for args in [
        &["show"][..],
        &["show", "--stat"],
        &["show", "--name-only", "c7"],
        &["show", "c6"],
        &["show", "c1"],
        &["show", "-s", "c2"],
        &["show", "--oneline", "c2"],
        &["show", "--format=%h %s", "-s", "c2", "c4"],
        &["show", "c5"],
        &["show", "c5", "--stat"],
        &["show", "c4", "c3"],
        &["show", "--pretty=fuller", "c4"],
        &["show", "master~2"],
        &["show", "v1"],
        &["show", "v2"],
        &["show", "--stat", "v2"],
        &["show", "master:a.txt"],
        &["show", "master:sub/b.txt"],
        &["show", "master:sub"],
        &["show", "master:"],
        &["show", "side..master", "--oneline", "-s"],
        &["show", "master:nope"],
        &["show", "nope"],
    ] {
        b.same(args);
    }
}

// ---------------------------------------------------------------------------------- json / misc

#[tokio::test]
async fn json_prints_log_response_lines() {
    let b = standard().await;
    let out = b.asc(&["log", "--json", "-n", "3"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let lines: Vec<serde_json::Value> = out
        .stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1, "{}", out.stdout);
    let entries = lines[0]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert_ne!(entries[0]["id"], entries[1]["id"]);
    assert_eq!(entries[0]["parents"][0], entries[1]["id"]);
    assert!(
        entries[0]["git"].as_str().is_some_and(|g| g.len() == 28),
        "{}",
        entries[0]["git"]
    );
    assert_eq!(entries[0]["author"]["name"].as_str().unwrap(), "VGVzdA==");
    // The walk order is git's; the JSON has no extra page when everything fits.
    let all = b.asc(&["log", "--json"]);
    let line: serde_json::Value = serde_json::from_str(all.stdout.lines().next().unwrap()).unwrap();
    assert_eq!(line["entries"].as_array().unwrap().len(), 7);
    assert_eq!(line["count"].as_u64(), Some(7));
}

#[tokio::test]
async fn errors_match_git() {
    let b = standard().await;
    for args in [
        &["log", "nope"][..],
        &["log", "nope", "--"],
        &["log", "--date=bogus"],
        &["log", "--pretty=bogus"],
        &["log", "-n", "x"],
        &["log", "c9999..master"],
    ] {
        let want = b.git(args);
        let got = b.asc(args);
        assert_eq!(got.code, want.code, "{args:?}: {}", got.stderr);
        assert_eq!(
            got.stderr.replace("'asc <command>", "'git <command>"),
            want.stderr,
            "{args:?}"
        );
    }
    let unborn = Fixture::new().await;
    let r = run(unborn.asc().args(["log"]));
    assert_eq!(r.code, 128);
    assert_eq!(
        r.stderr,
        "fatal: your current branch 'master' does not have any commits yet\n"
    );
}

#[tokio::test]
async fn unsupported_flags_are_named() {
    let b = standard().await;
    let r = b.asc(&["log", "--bogus-flag"]);
    assert_eq!(r.code, 129, "{}", r.stderr);
    assert!(r.stderr.contains("--bogus-flag"), "{}", r.stderr);
    let r = b.asc(&["log", "--format=%(bogus-atom-xyz)"]);
    assert_eq!(r.code, 129, "{}", r.stderr);
}

#[tokio::test]
async fn walk_reflogs_lists_branch_moves() {
    let b = standard().await;
    let r = b.asc(&["log", "-g", "--format=%gd|%gs|%h"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(
        r.stdout
            .lines()
            .next()
            .is_some_and(|l| l.starts_with("HEAD@{0}|")),
        "{}",
        r.stdout
    );
}
