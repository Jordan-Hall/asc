//! Learned prefetch profiles (SPEC §10.2 item 5b) and the `--auto` warm of item 5a, through the
//! built binary: `--learn` from standard input, `--list`, `--show`, `--forget`, `--profile`,
//! `--learned`, `--auto` with and without `.asc/prefetch`, and `--record` refused outside a
//! virtual workspace.

mod support;

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;

use support::{Sandbox, text};

struct Repo {
    sb: Sandbox,
    root: PathBuf,
}

impl Repo {
    fn new(files: &[(&str, &str)]) -> Self {
        let sb = Sandbox::new();
        fs::write(
            sb.user.join(".gitconfig"),
            "[user]\n\tname = T\n\temail = t@example.com\n",
        )
        .unwrap();
        let root = sb.dir("repo");
        let r = Self { sb, root };
        r.ok(&["init"]);
        for (name, body) in files {
            let path = r.root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
        r.ok(&["add", "."]);
        r.ok(&["commit", "-m", "base"]);
        r
    }

    fn run_with(&self, args: &[&str], input: &[u8]) -> (i32, String, String) {
        let mut child = self
            .sb
            .asc(&self.root)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        child.stdin.take().unwrap().write_all(input).unwrap();
        let out = child.wait_with_output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            text(&out.stdout),
            text(&out.stderr),
        )
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        self.run_with(args, b"")
    }

    fn ok(&self, args: &[&str]) -> String {
        let (code, out, err) = self.run(args);
        assert_eq!(code, 0, "asc {args:?}: {err}");
        out
    }
}

const FILES: &[(&str, &str)] = &[
    ("src/a.ts", "a\n"),
    ("src/b.ts", "b\n"),
    ("src/c.css", "c\n"),
    ("README.md", "r\n"),
];

#[test]
fn a_profile_is_learned_listed_shown_warmed_and_forgotten() {
    let r = Repo::new(FILES);
    let abs = r.root.join("src").join("b.ts");
    let input = format!(
        "src/a.ts\n{}\nsrc/missing.ts\n../outside\n.git/config\nsrc/a.ts\n",
        abs.display()
    );
    let (code, out, err) = r.run_with(&["prefetch", "--learn", "vite:app"], input.as_bytes());
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("learned 2 paths into vite:app"), "{out}");

    let shown = r.ok(&["prefetch", "--show", "vite:app"]);
    let mut lines: Vec<&str> = shown.lines().collect();
    lines.sort_unstable();
    assert_eq!(lines, ["src/a.ts", "src/b.ts"]);

    let listed = r.ok(&["prefetch", "--list"]);
    assert!(listed.starts_with("vite:app\t2\t"), "{listed}");
    let json: serde_json::Value =
        serde_json::from_str(&r.ok(&["prefetch", "--list", "--json"])).unwrap();
    assert_eq!(json[0]["name"], "vite:app");
    assert_eq!(json[0]["paths"], 2);

    let warmed = r.ok(&["prefetch", "--profile", "vite:app"]);
    assert!(warmed.contains("prefetched 2 files"), "{warmed}");
    let warmed = r.ok(&["prefetch", "--learned"]);
    assert!(warmed.contains("prefetched 2 files"), "{warmed}");
    assert_eq!(r.ok(&["prefetch", "--learned", "-q"]), "");

    // NUL-separated input merges into the profile; --replace starts it over.
    let (code, _, err) = r.run_with(&["prefetch", "--learn", "vite:app"], b"README.md\0");
    assert_eq!(code, 0, "{err}");
    assert_eq!(r.ok(&["prefetch", "--show", "vite:app"]).lines().count(), 3);
    let (code, _, err) = r.run_with(
        &["prefetch", "--learn", "vite:app", "--replace"],
        b"src/c.css\n",
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        r.ok(&["prefetch", "--show", "vite:app"]).trim(),
        "src/c.css"
    );

    assert!(
        r.ok(&["prefetch", "--forget", "vite:app"])
            .contains("forgot vite:app")
    );
    assert_eq!(r.ok(&["prefetch", "--list"]), "");
    let (code, _, err) = r.run(&["prefetch", "--show", "vite:app"]);
    assert_ne!(code, 0);
    assert!(err.contains("there is no profile vite:app"), "{err}");
    let (code, _, err) = r.run(&["prefetch", "--profile", "vite:app"]);
    assert_eq!(code, 1);
    assert!(err.contains("no such profile"), "{err}");
}

#[test]
fn learned_days_bounds_the_learned_warm() {
    let r = Repo::new(FILES);
    let (code, _, err) = r.run_with(&["prefetch", "--learn", "p"], b"src/a.ts\n");
    assert_eq!(code, 0, "{err}");
    r.ok(&["config", "asc.prefetch.learnedDays", "0"]);
    // A profile used this second is still inside a window of zero days.
    assert!(
        r.ok(&["prefetch", "--learned"])
            .contains("prefetched 1 files")
    );
    r.ok(&["config", "asc.prefetch.learnedDays", "soon"]);
    let (code, _, err) = r.run(&["prefetch", "--learned"]);
    assert_ne!(code, 0);
    assert!(err.contains("asc.prefetch.learnedDays"), "{err}");
}

#[test]
fn auto_warms_the_default_globs_or_the_prefetch_file() {
    let r = Repo::new(FILES);
    // The default globs take the sources and skip the css and the readme.
    let o = r.ok(&["prefetch", "--auto"]);
    assert!(o.contains("prefetched 2 files"), "{o}");

    fs::create_dir_all(r.root.join(".asc")).unwrap();
    fs::write(
        r.root.join(".asc/prefetch"),
        "# styles and docs\n**/*.css\n*.md\n!README.md\n",
    )
    .unwrap();
    r.ok(&["add", ".asc/prefetch"]);
    r.ok(&["commit", "-m", "prefetch globs"]);
    let o = r.ok(&["prefetch", "--auto"]);
    assert!(o.contains("prefetched 1 files"), "{o}");
}

#[test]
fn record_needs_a_virtual_workspace() {
    let r = Repo::new(FILES);
    let (code, _, err) = r.run(&["prefetch", "--record", "p", "--", "asc", "status"]);
    assert_ne!(code, 0);
    assert!(err.contains("--record needs a virtual workspace"), "{err}");
    assert!(err.contains("--learn"), "{err}");
}

#[test]
fn bad_arguments_are_usage_errors() {
    let r = Repo::new(FILES);
    for args in [
        &["prefetch", "--learn", "a b"][..],
        &["prefetch", "--learn", "x", "src"],
        &["prefetch", "--list", "--show", "x"],
        &["prefetch", "src", "--replace"],
        &["prefetch", "--record", "x"],
    ] {
        let (code, _, err) = r.run(args);
        assert_ne!(code, 0, "{args:?}");
        assert!(err.contains("usage: asc prefetch"), "{args:?}: {err}");
    }
}
