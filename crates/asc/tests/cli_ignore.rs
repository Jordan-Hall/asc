//! U24b acceptance: `check-ignore`, `check-attr`, `grep` and `clean`, driven through the built
//! binary against real git (via `RealGit`) on identical files. The asc side builds its fixture
//! directly through the engine and `state.db` (PLAN "Acceptance before U25"); no sibling command
//! is used.

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use asc_git::realgit::RealGit;
use support::{Fixture, text};

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn finish(mut cmd: Command, stdin: &[u8]) -> Out {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn");
    let mut input = child.stdin.take().expect("stdin");
    let bytes = stdin.to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = input.write_all(&bytes);
    });
    let out = child.wait_with_output().expect("wait");
    feeder.join().unwrap();
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: text(&out.stdout),
        stderr: text(&out.stderr),
    }
}

/// An asc workspace and a real git repository holding the same files.
struct Rig {
    f: Fixture,
    git: PathBuf,
    git_bin: PathBuf,
}

impl Rig {
    async fn new() -> Self {
        let f = Fixture::new().await;
        let git = f.sandbox.dir("gitwork");
        let git_bin = RealGit::resolve(None, None)
            .expect("real git")
            .path()
            .to_owned();
        let rig = Self { f, git, git_bin };
        for args in [
            &["init", "-q"][..],
            &["config", "core.autocrlf", "false"],
            &["config", "user.name", "T"],
            &["config", "user.email", "t@example.com"],
        ] {
            assert_eq!(rig.git(args).code, 0);
        }
        rig
    }

    fn git_in(&self, sub: &str, args: &[&str], stdin: &[u8]) -> Out {
        let dir = self.git.join(sub);
        let mut cmd = self.f.sandbox.command(&self.git_bin, &dir);
        cmd.args(args);
        finish(cmd, stdin)
    }

    fn asc_in(&self, sub: &str, args: &[&str], stdin: &[u8]) -> Out {
        let dir = self.f.root.join(sub);
        let mut cmd = self.f.sandbox.asc(&dir);
        cmd.args(args);
        finish(cmd, stdin)
    }

    fn git(&self, args: &[&str]) -> Out {
        self.git_in("", args, b"")
    }

    fn asc(&self, args: &[&str]) -> Out {
        self.asc_in("", args, b"")
    }

    /// Writes `rel` in both work trees.
    fn write(&self, rel: &str, bytes: &[u8]) {
        for root in [&self.f.root, &self.git] {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, bytes).unwrap();
        }
    }

    fn mkdir(&self, rel: &str) {
        for root in [&self.f.root, &self.git] {
            fs::create_dir_all(root.join(rel)).unwrap();
        }
    }

    /// Commits `files` in both: the same HEAD tree, nothing staged.
    async fn seed(&self, files: &[(&str, &[u8])]) {
        for (rel, bytes) in files {
            self.write(rel, bytes);
        }
        let c = self.f.commit(&[], files).await;
        self.f.set_branch("master", c).await;
        self.f.checkout_branch("master", Some(c)).await;
        assert_eq!(self.git(&["add", "-A"]).code, 0);
        assert_eq!(self.git(&["commit", "-qm", "seed"]).code, 0);
    }

    /// Runs `args` on both sides in `sub` and asserts the same exit code and standard output (and
    /// standard error when `err`).
    fn same(&self, sub: &str, args: &[&str], stdin: &[u8], err: bool) -> Out {
        let (c, g) = (self.asc_in(sub, args, stdin), self.git_in(sub, args, stdin));
        let what = format!("{args:?} in {sub:?}");
        assert_eq!(
            c.stdout, g.stdout,
            "stdout of {what}\nasc stderr: {}",
            c.stderr
        );
        assert_eq!(
            c.code, g.code,
            "exit code of {what}\nasc stderr: {}",
            c.stderr
        );
        if err {
            assert_eq!(c.stderr, g.stderr, "stderr of {what}");
        }
        c
    }

    /// The files below each work tree, relative and sorted, without the metadata directories.
    fn files(&self, git_side: bool) -> BTreeSet<String> {
        let root = if git_side { &self.git } else { &self.f.root };
        let mut out = BTreeSet::new();
        walk(root, root, &mut out);
        out
    }
}

fn walk(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if rel == ".git" || rel == ".asc" {
            continue;
        }
        if entry.file_type().unwrap().is_dir() {
            out.insert(format!("{rel}/"));
            walk(root, &entry.path(), out);
        } else {
            out.insert(rel);
        }
    }
}

// ---------------------------------------------------------------- check-ignore

async fn ignore_rig() -> Rig {
    let r = Rig::new().await;
    r.seed(&[("tracked.log", b"t\n"), ("sub/keep.txt", b"k\n")])
        .await;
    r.write(".gitignore", b"build/\n*.log\n!keep.log\n/rooted\n*.tmp\n");
    r.write("sub/.gitignore", b"x.tmp\n!y.tmp\nlocal/\n");
    for f in [
        "a.log",
        "keep.log",
        "rooted",
        "sub/rooted",
        "build/x.o",
        "sub/x.tmp",
        "sub/y.tmp",
        "sub/local/f",
        "a.tmp",
        "sp ace.log",
    ] {
        r.write(f, b"x\n");
    }
    r
}

const IGNORE_PATHS: &[&str] = &[
    "build",
    "build/",
    "build/x.o",
    "nonexist/",
    "build2/",
    "a.log",
    "keep.log",
    "sub/x.tmp",
    "sub/y.tmp",
    "tracked.log",
    "rooted",
    "sub/rooted",
    "nothing",
    "sub/local/f",
    "sub/local",
    "a.tmp",
    "sp ace.log",
    "sub/z.tmp",
    "./a.log",
    "sub/../a.log",
    "sub",
    "sub/keep.txt",
];

#[tokio::test]
async fn check_ignore_matches_git() {
    let r = ignore_rig().await;
    for flags in [
        &[][..],
        &["-v"],
        &["-v", "-n"],
        &["--no-index"],
        &["-v", "--no-index"],
        &["-v", "-n", "--no-index"],
    ] {
        let mut args = vec!["check-ignore"];
        args.extend(flags);
        args.extend(IGNORE_PATHS);
        r.same("", &args, b"", false);
        for p in IGNORE_PATHS {
            let mut one = vec!["check-ignore"];
            one.extend(flags);
            one.push(p);
            r.same("", &one, b"", false);
        }
    }
    for p in IGNORE_PATHS {
        r.same("", &["check-ignore", "-q", p], b"", false);
    }
}

#[tokio::test]
async fn check_ignore_in_a_subdirectory_prints_names_as_typed() {
    let r = ignore_rig().await;
    r.same(
        "sub",
        &[
            "check-ignore",
            "-v",
            "-n",
            "x.tmp",
            "y.tmp",
            "../a.log",
            "local/f",
            "../tracked.log",
            "rooted",
        ],
        b"",
        false,
    );
    r.same(
        "sub",
        &["check-ignore", "-v", "../build/x.o", "../sub/x.tmp"],
        b"",
        false,
    );
    assert_eq!(
        r.asc_in("sub", &["check-ignore", "../../outside"], b"").code,
        128
    );
    assert_eq!(
        r.git_in("sub", &["check-ignore", "../../outside"], b"")
            .code,
        128
    );
}

#[tokio::test]
async fn check_ignore_stdin_and_nul_forms_match_git() {
    let r = ignore_rig().await;
    let lines = b"a.log\nnothing\nkeep.log\n\"sp ace.log\"\nsub/x.tmp\nbuild/\n";
    for flags in [
        &["--stdin"][..],
        &["--stdin", "-v"],
        &["--stdin", "-v", "-n"],
        &["--stdin", "--no-index", "-v", "-n"],
    ] {
        let mut args = vec!["check-ignore"];
        args.extend(flags);
        r.same("", &args, lines, false);
    }
    let nul = b"a.log\0nothing\0keep.log\0sp ace.log\0sub/x.tmp\0build/\0";
    for flags in [
        &["--stdin", "-z"][..],
        &["--stdin", "-z", "-v"],
        &["--stdin", "-z", "-v", "-n"],
    ] {
        let mut args = vec!["check-ignore"];
        args.extend(flags);
        r.same("", &args, nul, false);
    }
}

#[tokio::test]
async fn check_ignore_misuse_matches_git() {
    let r = ignore_rig().await;
    for args in [
        &["check-ignore"][..],
        &["check-ignore", "-q", "-v", "a.log"],
        &["check-ignore", "-q", "a.log", "nothing"],
        &["check-ignore", "-z", "a.log"],
        &["check-ignore", "-n", "a.log"],
        &["check-ignore", "--stdin", "a.log"],
    ] {
        r.same("", args, b"", true);
    }
}

#[tokio::test]
async fn ascignore_replaces_gitignore_and_is_named_by_v() {
    let r = Rig::new().await;
    fs::write(r.f.root.join(".gitignore"), "*.log\n").unwrap();
    fs::write(r.f.root.join(".ascignore"), "# mine\n*.tmp\n!keep.tmp\n").unwrap();
    for f in ["a.log", "a.tmp", "keep.tmp"] {
        fs::write(r.f.root.join(f), "x").unwrap();
    }
    let out = r.asc(&["check-ignore", "-v", "-n", "a.log", "a.tmp", "keep.tmp"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "::\ta.log\n.ascignore:2:*.tmp\ta.tmp\n.ascignore:3:!keep.tmp\tkeep.tmp\n"
    );
    assert_eq!(
        out.stderr,
        "warning: asc: .gitignore is not read because .ascignore exists (rename or merge it: asc migrate ignore)\n"
    );
    // The same contents in both files: no shadow warning.
    fs::write(r.f.root.join(".gitignore"), "# mine\n*.tmp\n!keep.tmp\n").unwrap();
    let again = r.asc(&["check-ignore", "a.tmp"]);
    assert_eq!(
        (again.code, again.stdout.as_str(), again.stderr.as_str()),
        (0, "a.tmp\n", "")
    );
}

#[tokio::test]
async fn exclude_files_are_named_with_their_location() {
    let r = Rig::new().await;
    fs::create_dir_all(r.f.root.join(".asc/info")).unwrap();
    fs::write(r.f.root.join(".asc/info/exclude"), "scratch/\n*.bak\n").unwrap();
    let out = r.asc(&[
        "check-ignore",
        "-v",
        "x.bak",
        "scratch/f",
        ".asc/info/exclude",
    ]);
    assert_eq!(
        out.stdout,
        ".asc/info/exclude:2:*.bak\tx.bak\n.asc/info/exclude:1:scratch/\tscratch/f\n:0:.asc/info/\t.asc/info/exclude\n"
    );
}

#[tokio::test]
async fn check_ignore_refuses_a_path_beyond_a_link() {
    let r = Rig::new().await;
    let outside = r.f.sandbox.dir("outside");
    fs::write(outside.join("f"), "x").unwrap();
    if !link_dir(&outside, &r.f.root.join("link")) {
        return;
    }
    let out = r.asc(&["check-ignore", "link/f"]);
    assert_eq!(out.code, 128);
    assert!(
        out.stderr.contains("is beyond a symbolic link"),
        "{}",
        out.stderr
    );
}

/// Makes `link` a directory link to `target`; false when the OS refuses.
fn link_dir(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    return std::os::unix::fs::symlink(target, link).is_ok();
    #[cfg(windows)]
    return std::os::windows::fs::symlink_dir(target, link).is_ok()
        // Without the symlink privilege a junction is the same kind of reparse point.
        || Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
    #[cfg(not(any(unix, windows)))]
    false
}

// ---------------------------------------------------------------- check-attr

async fn attr_rig() -> Rig {
    let r = Rig::new().await;
    r.write(
        ".gitattributes",
        b"*.txt text eol=lf\n*.bin binary -diff\n/tr.txt custom=yes -foo bar=\nsub/** x=1\n[attr]mymacro text -binary q=z\n*.mm mymacro\n",
    );
    r.write("sub/.gitattributes", b"*.txt -text\nspecial !eol\n");
    r
}

const ATTR_PATHS: &[&str] = &[
    "tr.txt",
    "a.bin",
    "sub/f.txt",
    "sub/x.mm",
    "x.mm",
    "plain",
    "missing/dir/file.bin",
    "sub/special",
    "sub/deep/n.txt",
    "sub",
];

#[tokio::test]
async fn check_attr_matches_git() {
    let r = attr_rig().await;
    r.same(
        "",
        &[&["check-attr", "-a", "--"][..], ATTR_PATHS].concat(),
        b"",
        false,
    );
    r.same(
        "",
        &[&["check-attr", "-a"][..], ATTR_PATHS].concat(),
        b"",
        false,
    );
    r.same(
        "",
        &[
            &[
                "check-attr",
                "text",
                "eol",
                "diff",
                "foo",
                "custom",
                "binary",
                "bar",
                "x",
                "nosuch",
                "--",
            ][..],
            ATTR_PATHS,
        ]
        .concat(),
        b"",
        false,
    );
    r.same(
        "",
        &[&["check-attr", "text", "text", "Text"][..], &["tr.txt"]].concat(),
        b"",
        false,
    );
    r.same(
        "",
        &[
            "check-attr",
            "-z",
            "text",
            "eol",
            "--",
            "tr.txt",
            "sub/f.txt",
        ],
        b"",
        false,
    );
    r.same("", &["check-attr", "-z", "-a", "tr.txt"], b"", false);
    r.same(
        "sub",
        &["check-attr", "-a", "f.txt", "../tr.txt", "x.mm"],
        b"",
        false,
    );
    r.same("sub", &["check-attr", "text", "tr.txt"], b"", false);
}

#[tokio::test]
async fn check_attr_stdin_forms_match_git() {
    let r = attr_rig().await;
    let lines = b"tr.txt\na.bin\n\"sub/f.txt\"\nmissing\n";
    r.same("", &["check-attr", "--stdin", "text", "diff"], lines, false);
    r.same("", &["check-attr", "--stdin", "-a"], lines, false);
    let nul = b"tr.txt\0a.bin\0sub/f.txt\0";
    r.same(
        "",
        &["check-attr", "--stdin", "-z", "text", "diff"],
        nul,
        false,
    );
    r.same("", &["check-attr", "--stdin", "-z", "-a"], nul, false);
}

#[tokio::test]
async fn check_attr_misuse_is_a_usage_error() {
    let r = attr_rig().await;
    for args in [
        &["check-attr"][..],
        &["check-attr", "--"],
        &["check-attr", "text"],
        &["check-attr", "-a", "text", "--", "f"],
        &["check-attr", "--stdin", "text", "--", "f"],
    ] {
        let (c, g) = (r.asc_in("", args, b""), r.git_in("", args, b""));
        assert_eq!((c.code, g.code), (129, 129), "{args:?}: {}", c.stderr);
        assert_eq!(c.stderr.lines().next(), g.stderr.lines().next(), "{args:?}");
    }
    let c = r.asc(&["check-attr", "text", "--", "../x"]);
    assert_eq!(c.code, 128, "{}", c.stderr);
}

#[tokio::test]
async fn ascattributes_replaces_gitattributes_per_directory() {
    let r = Rig::new().await;
    fs::write(r.f.root.join(".gitattributes"), "* text\n").unwrap();
    fs::create_dir_all(r.f.root.join("sub")).unwrap();
    fs::write(r.f.root.join("sub/.gitattributes"), "* text\n").unwrap();
    fs::write(r.f.root.join("sub/.ascattributes"), "* -text mine=1\n").unwrap();
    let out = r.asc(&[
        "check-attr",
        "text",
        "mine",
        "--",
        "x",
        "sub/x",
        "sub/deeper/x",
    ]);
    assert_eq!(
        out.stdout,
        "x: text: set\nx: mine: unspecified\nsub/x: text: unset\nsub/x: mine: 1\nsub/deeper/x: text: unset\nsub/deeper/x: mine: 1\n"
    );
    let info = r.f.root.join(".git");
    assert!(info.exists());
}

#[tokio::test]
async fn attribute_files_behind_a_link_are_not_read() {
    let r = Rig::new().await;
    let outside = r.f.sandbox.dir("attr-outside");
    fs::write(outside.join(".gitattributes"), "* secret=yes\n").unwrap();
    if !link_dir(&outside, &r.f.root.join("link")) {
        return;
    }
    let out = r.asc(&["check-attr", "secret", "--", "link/f"]);
    assert_eq!(
        out.stdout, "link/f: secret: unspecified\n",
        "{}",
        out.stderr
    );
}

// ---------------------------------------------------------------- grep

async fn grep_rig() -> Rig {
    let r = Rig::new().await;
    r.seed(&[
        ("a.txt", b"Hello World\nfoo bar\nfoobar\nthe foo\nlast"),
        ("src/main.rs", b"fn main() {\n    println!(\"hello\");\n}\n"),
        ("src/lib/mod.rs", b"pub fn foo() {}\npub fn foo_bar() {}\n"),
        ("bin.dat", b"foo\0bar\nfoo again\n"),
        ("crlf.txt", b"foo\r\nbar\r\n"),
        ("gone.txt", b"foo gone\n"),
        ("empty", b""),
    ])
    .await;
    // Dirty: a changed tracked file, a deleted one and an untracked one.
    r.write(
        "a.txt",
        b"Hello World\nfoo bar\nfoobar\nthe foo\nlast\nfoo dirty line\n",
    );
    for root in [&r.f.root, &r.git] {
        fs::remove_file(root.join("gone.txt")).unwrap();
    }
    r.write("untracked.txt", b"foo untracked\n");
    r
}

#[tokio::test]
async fn grep_matches_git() {
    let r = grep_rig().await;
    let cases: &[&[&str]] = &[
        &["grep", "foo"],
        &["grep", "-n", "foo"],
        &["grep", "-i", "hello"],
        &["grep", "-ni", "HELLO"],
        &["grep", "-w", "foo"],
        &["grep", "-nw", "foo"],
        &["grep", "-l", "foo"],
        &["grep", "-c", "foo"],
        &["grep", "-v", "foo", "--", "a.txt"],
        &["grep", "-nv", "o", "--", "src"],
        &["grep", "-e", "hello", "-e", "foo_"],
        &["grep", "-F", "-n", "println!("],
        &["grep", "-F", "a.b"],
        &["grep", "-E", "fo+ba?r"],
        &["grep", "-E", "^(Hello|the)"],
        &["grep", "foo\\|Hello"],
        &["grep", "fo\\+"],
        &["grep", "^pub fn \\(foo\\|foo_bar\\)"],
        &["grep", "-n", "foo", "--", "src"],
        &["grep", "-n", "foo", "--", "src/lib", "a.txt"],
        &["grep", "-n", "foo", "--", "*.rs"],
        &["grep", "-n", "foo", "--", "nonexistent"],
        &["grep", "-nE", "fn\\s+\\w+"],
        &["grep", "-q", "foo"],
        &["grep", "-q", "zzzz-not-there"],
        &["grep", "zzzz-not-there"],
        &["grep", "-c", "zzzz-not-there"],
        &["grep", "-l", "-e", "foo", "-e", "Hello", "--", "."],
        &["grep", "-n", "", "--", "empty", "crlf.txt"],
        &["grep", "-w", "Hello World"],
        &["grep", "-n", "[[:upper:]]"],
        &["grep", "-n", "[]x]"],
    ];
    for args in cases {
        r.same("", args, b"", false);
    }
    // In a subdirectory: only that directory is searched, names are relative.
    r.same("src", &["grep", "-n", "foo"], b"", false);
    r.same("src", &["grep", "-n", "foo", "--", "../a.txt"], b"", false);
    r.same("src/lib", &["grep", "-c", "foo"], b"", false);
}

#[tokio::test]
async fn grep_misuse_and_limits() {
    let r = grep_rig().await;
    for args in [
        &["grep"][..],
        &["grep", "-E", "("],
        &["grep", "foo", "nonexistent-path"],
    ] {
        let (c, g) = (r.asc_in("", args, b""), r.git_in("", args, b""));
        assert_eq!(c.code, g.code, "{args:?}: {}", c.stderr);
        assert!(c.stderr.starts_with("fatal: "), "{args:?}: {}", c.stderr);
    }
    // Back-references are refused, not misread.
    let out = r.asc(&["grep", "-E", "(fo)\\1"]);
    assert_eq!(out.code, 128);
    assert!(
        out.stderr.contains("back-references are not supported"),
        "{}",
        out.stderr
    );
    // A pattern that blows the expression size limit is an error, not a hang.
    let big = "a{1000}{1000}{1000}";
    assert_eq!(r.asc(&["grep", "-E", big]).code, 128);
}

#[tokio::test]
async fn grep_does_not_read_through_links() {
    let r = Rig::new().await;
    let outside = r.f.sandbox.dir("grep-outside");
    fs::write(outside.join("secret"), "needle outside\n").unwrap();
    let c =
        r.f.commit(
            &[],
            &[
                ("link/secret", b"needle tracked\n"),
                ("plain", b"needle plain\n"),
            ],
        )
        .await;
    r.f.set_branch("master", c).await;
    r.f.checkout_branch("master", Some(c)).await;
    fs::write(r.f.root.join("plain"), "needle plain\n").unwrap();
    if !link_dir(&outside, &r.f.root.join("link")) {
        return;
    }
    let out = r.asc(&["grep", "needle"]);
    assert_eq!(out.stdout, "plain:needle plain\n", "{}", out.stderr);
}

// ---------------------------------------------------------------- clean

async fn clean_rig() -> Rig {
    let r = Rig::new().await;
    r.seed(&[
        (".gitignore", b"*.log\nbuild/\n"),
        ("tr.txt", b"t\n"),
        ("tdir/f", b"x\n"),
    ])
    .await;
    for (f, body) in [
        ("un.txt", "a"),
        ("u1/deep/f", "a"),
        ("u2/g", "a"),
        ("a.log", "a"),
        ("mixed/keep.log", "a"),
        ("mixed/new.txt", "a"),
        ("build/o", "a"),
        ("tdir/untracked", "a"),
        ("tdir/ig.log", "a"),
        ("sp ace/f g", "a"),
        ("allignored/x.log", "a"),
    ] {
        r.write(f, body.as_bytes());
    }
    for d in ["empty", "ign", "tdir/emptysub", "u3/emptysub"] {
        r.mkdir(d);
    }
    r
}

const CLEAN_DRY: &[&[&str]] = &[
    &["clean", "-n"],
    &["clean", "-nd"],
    &["clean", "-ndx"],
    &["clean", "-ndX"],
    &["clean", "-nX"],
    &["clean", "-nx"],
    &["clean", "-nq"],
    &["clean", "-nd", "tdir"],
    &["clean", "-nd", "u1"],
    &["clean", "-nd", "u1/"],
    &["clean", "-n", "--", "u1/deep"],
    &["clean", "-n", "u1/deep/f"],
    &["clean", "-n", "u1"],
    &["clean", "-nx", "u1"],
    &["clean", "-n", "u1/*"],
    &["clean", "-n", "u1/d*"],
    &["clean", "-n", "mixed"],
    &["clean", "-nx", "mixed"],
    &["clean", "-n", "mixed/new.txt"],
    &["clean", "-n", "empty"],
    &["clean", "-n", "ign"],
    &["clean", "-n", "build"],
    &["clean", "-nx", "build"],
    &["clean", "-nx", "build/o"],
    &["clean", "-n", "a.log"],
    &["clean", "-nx", "a.log"],
    &["clean", "-n", "a.*"],
    &["clean", "-nx", "a.*"],
    &["clean", "-n", "*.txt"],
    &["clean", "-nx", "*.txt"],
    &["clean", "-n", "."],
    &["clean", "-nx", "."],
    &["clean", "-nd", "nonexist"],
    &["clean", "-nd", "tdir", "u2", "un.txt"],
    &["clean", "-ndX", "allignored"],
    &["clean", "-ndx", "tdir"],
];

#[tokio::test]
async fn clean_dry_run_matches_git() {
    let r = clean_rig().await;
    for args in CLEAN_DRY {
        r.same("", args, b"", false);
    }
    r.same("tdir", &["clean", "-nd"], b"", false);
    r.same("tdir", &["clean", "-ndx"], b"", false);
    r.same("mixed", &["clean", "-nx"], b"", false);
    // Nothing was removed by the dry runs.
    assert_eq!(r.files(false), r.files(true));
}

#[tokio::test]
async fn clean_removes_what_git_removes() {
    for flags in [
        &["-f"][..],
        &["-fd"],
        &["-fdx"],
        &["-fdX"],
        &["-fX"],
        &["-fx"],
        &["-fdq"],
        &["-fd", "u1"],
        &["-fdx", "tdir"],
        &["-f", "mixed"],
        &["-fx", "mixed"],
        &["-f", "u2"],
        &["-f", "empty", "ign"],
    ] {
        let r = clean_rig().await;
        let mut args = vec!["clean"];
        args.extend(flags);
        r.same("", &args, b"", false);
        assert_eq!(r.files(false), r.files(true), "tree after {flags:?}");
    }
}

#[tokio::test]
async fn clean_refuses_without_force() {
    let r = clean_rig().await;
    let before = r.files(false);
    r.same("", &["clean"], b"", true);
    r.same("", &["clean", "-d"], b"", true);
    assert_eq!(r.files(false), before);
    let both = r.asc(&["clean", "-nx", "-X"]);
    assert_eq!(both.code, 128);
    assert!(
        both.stderr.contains("cannot be used together"),
        "{}",
        both.stderr
    );
    // `clean.requireForce=false` lifts the need for -f.
    assert_eq!(r.asc(&["config", "clean.requireForce", "false"]).code, 0);
    assert_eq!(r.git(&["config", "clean.requireForce", "false"]).code, 0);
    r.same("", &["clean"], b"", false);
    assert_eq!(r.files(false), r.files(true));
}

#[tokio::test]
async fn clean_never_follows_links_or_touches_metadata() {
    let r = clean_rig().await;
    let outside = r.f.sandbox.dir("clean-outside");
    fs::create_dir_all(outside.join("inner")).unwrap();
    fs::write(outside.join("inner/keep"), "precious").unwrap();
    fs::write(outside.join("top"), "precious").unwrap();
    let linked = link_dir(&outside, &r.f.root.join("linkdir"));
    fs::create_dir_all(r.f.root.join(".asc")).unwrap();
    fs::write(r.f.root.join(".asc/redirections"), "node_modules\n").unwrap();
    fs::create_dir_all(r.f.root.join("node_modules/pkg")).unwrap();
    fs::write(r.f.root.join("node_modules/pkg/index.js"), "x").unwrap();
    fs::create_dir_all(r.f.root.join("nested/.git")).unwrap();
    fs::write(r.f.root.join("nested/f"), "x").unwrap();
    let out = r.asc(&["clean", "-fdx"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(
        outside.join("inner/keep").exists() && outside.join("top").exists(),
        "files behind a link survive"
    );
    if linked {
        assert!(
            !r.f.root.join("linkdir").exists(),
            "the link itself is removed"
        );
    }
    assert!(
        r.f.root.join(".asc/redirections").exists(),
        "the .asc metadata directory is never removed"
    );
    assert!(
        r.f.root.join("node_modules/pkg/index.js").exists(),
        "redirected names are never walked"
    );
    assert!(
        r.f.root.join("nested/f").exists(),
        "a nested repository survives a single -f"
    );
    assert!(!r.f.root.join("un.txt").exists() && !r.f.root.join("u1").exists());
    assert!(
        r.f.root.join("tr.txt").exists() && r.f.root.join("tdir/f").exists(),
        "tracked files stay"
    );
    // `-ff` asks for the nested repository too; asc never deletes a `.git`, so it stays and asc says
    // so. What is behind a link stays as well.
    let forced = r.asc(&["clean", "-ffdx"]);
    assert_eq!(forced.code, 0, "{}", forced.stderr);
    assert!(r.f.root.join("nested/f").exists());
    assert!(
        forced
            .stderr
            .contains("not removing the nested repository nested/"),
        "{}",
        forced.stderr
    );
    assert!(outside.join("inner/keep").exists());
}

#[tokio::test]
async fn clean_keeps_the_current_directory() {
    let r = clean_rig().await;
    let out = r.asc_in("u1/deep", &["clean", "-fd"], b"");
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(
        r.f.root.join("u1/deep").is_dir(),
        "the current directory is not removed"
    );
    assert!(
        out.stdout
            .contains("Refusing to remove current working directory"),
        "{}",
        out.stdout
    );
}

#[tokio::test]
async fn clean_keeps_the_empty_directory_when_removing_it_would_take_an_unnamed_parent() {
    let r = Rig::new().await;
    r.seed(&[(
        "tr.txt", b"t
",
    )])
    .await;
    fs::create_dir_all(r.f.root.join("outer/inner")).unwrap();
    fs::write(r.f.root.join("outer/inner/f"), "x").unwrap();
    // `outer` is untracked and only `outer/inner` is named. The gate removes a directory only as
    // the parent of a path and then every parent it leaves empty, so `outer` would go too: asc
    // keeps the emptied directory and says so, where git removes `inner` and keeps `outer`.
    let out = r.asc(&["clean", "-f", "outer/inner"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(
        out.stderr
            .contains("warning: asc: left the empty directory outer/inner/"),
        "{}",
        out.stderr
    );
    assert!(!r.f.root.join("outer/inner/f").exists());
    assert!(r.f.root.join("outer/inner").is_dir());
    // With another entry in `outer` the removal is exactly git's.
    fs::write(r.f.root.join("outer/other"), "x").unwrap();
    let again = r.asc(&["clean", "-f", "outer/inner"]);
    assert_eq!(again.code, 0, "{}", again.stderr);
    assert!(!r.f.root.join("outer/inner").exists() && r.f.root.join("outer/other").exists());
}
