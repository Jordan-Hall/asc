//! `asc cache dotnet` and `asc cache pytest` over stand-in `dotnet` and `python` scripts (they
//! count their runs): a build or test run that passed in one work tree is restored in a second;
//! a changed project, a changed SDK, a command asc does not cache and a run that depends on the
//! last run all run the tool. The stand-ins are shell scripts, so the cases run on Unix.

mod support;

use std::path::{Path, PathBuf};
use std::process::Output;

use support::{Sandbox, asc_binary, text};

const DOTNET: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "${FAKE_SDK:-8.0.100}"; exit 0; fi
echo "$1" >> "$COUNT"
case "$1" in
  build) mkdir -p app/bin/Debug && cat app/app.csproj > app/bin/Debug/app.dll; echo "Build succeeded." ;;
  test) mkdir -p tr && echo '<TestRun/>' > tr/run.trx; echo "Passed!  - Failed: 0, Passed: 3" ;;
  *) echo "other $1" ;;
esac
"#;

const PYTHON: &str = r#"#!/bin/sh
if [ "$1" = "-c" ]; then echo "3.12.4"; echo "['pytest==8.3.0']"; exit 0; fi
shift 2
echo "pytest $*" >> "$COUNT"
for a in "$@"; do
  case "$a" in --junitxml=*) f="${a#--junitxml=}"; mkdir -p "$(dirname "$f")"; echo '<testsuite/>' > "$f" ;; esac
done
echo "3 passed in 0.01s"
[ -f fail.txt ] && exit 1
exit 0
"#;

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    p
}

struct Env {
    sb: Sandbox,
    bin: PathBuf,
    count: PathBuf,
}

impl Env {
    fn new() -> Self {
        let sb = Sandbox::new();
        let bin = sb.dir("fakebin");
        script(&bin, "dotnet", DOTNET);
        script(&bin, "python3", PYTHON);
        let count = sb.dir("counter").join("count.txt");
        Self { sb, bin, count }
    }

    fn asc(&self, cwd: &Path, sdk: &str, args: &[&str]) -> Output {
        let path = std::env::join_paths(std::iter::once(self.bin.clone()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();
        self.sb
            .asc(cwd)
            .env("PATH", path)
            .env("COUNT", &self.count)
            .env("FAKE_SDK", sdk)
            .env_remove("CI")
            .args(args)
            .output()
            .expect("spawn asc")
    }

    fn ok(&self, cwd: &Path, args: &[&str]) -> Output {
        let out = self.asc(cwd, "8.0.100", args);
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        out
    }

    fn runs(&self) -> Vec<String> {
        std::fs::read_to_string(&self.count)
            .map(|s| s.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    fn repo(&self, name: &str) -> PathBuf {
        let r = self.sb.dir(name);
        self.ok(&r, &["init", "-q"]);
        self.ok(&r, &["config", "asc.buildcache.remote", "off"]);
        std::fs::write(r.join(".gitignore"), "bin/\nobj/\ntr/\nout/\n").unwrap();
        std::fs::create_dir_all(r.join("app")).unwrap();
        std::fs::write(r.join("app/app.csproj"), "<Project />\n").unwrap();
        std::fs::write(r.join("app/Program.cs"), "class P {}\n").unwrap();
        std::fs::write(r.join("global.json"), "{}\n").unwrap();
        std::fs::create_dir_all(r.join("tests")).unwrap();
        std::fs::write(r.join("pyproject.toml"), "[project]\nname = \"x\"\n").unwrap();
        std::fs::write(
            r.join("tests/test_sum.py"),
            "def test_sum():\n    assert 1 + 2 == 3\n",
        )
        .unwrap();
        r
    }
}

#[test]
fn dotnet_build_and_test_are_restored_in_another_work_tree() {
    if asc_binary().is_none() || cfg!(windows) {
        return;
    }
    let e = Env::new();
    let a = e.repo("a");
    let build = e.ok(&a, &["cache", "dotnet", "--", "build"]);
    assert!(text(&build.stdout).contains("Build succeeded."));
    e.ok(
        &a,
        &["cache", "dotnet", "--", "test", "--results-directory", "tr"],
    );
    assert_eq!(e.runs(), ["build", "test"]);

    let b = e.repo("b");
    let hit = e.ok(&b, &["cache", "dotnet", "--", "build"]);
    assert!(
        text(&hit.stdout).contains("Build succeeded."),
        "output replayed"
    );
    assert!(
        text(&hit.stderr).contains("asc cache: hit dotnet-build"),
        "{}",
        text(&hit.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(b.join("app/bin/Debug/app.dll")).unwrap(),
        "<Project />\n"
    );
    e.ok(
        &b,
        &["cache", "dotnet", "--", "test", "--results-directory", "tr"],
    );
    assert!(b.join("tr/run.trx").is_file());
    assert_eq!(e.runs().len(), 2, "both restored");

    // A changed project, or another SDK, builds again.
    std::fs::write(b.join("app/Program.cs"), "class Q {}\n").unwrap();
    e.ok(&b, &["cache", "dotnet", "--", "build"]);
    assert_eq!(e.runs().len(), 3);
    let other_sdk = e.asc(&a, "9.0.100", &["cache", "dotnet", "--", "build"]);
    assert!(other_sdk.status.success());
    assert_eq!(e.runs().len(), 4);

    // Commands asc does not cache run every time.
    for _ in 0..2 {
        let out = e.ok(&a, &["cache", "dotnet", "--", "restore"]);
        assert!(text(&out.stdout).contains("other restore"));
    }
    assert_eq!(e.runs().len(), 6);
}

#[test]
fn pytest_runs_are_restored_unless_they_depend_on_the_last_run() {
    if asc_binary().is_none() || cfg!(windows) {
        return;
    }
    let e = Env::new();
    let a = e.repo("a");
    let first = e.ok(
        &a,
        &["cache", "pytest", "--", "--junitxml=out/junit.xml", "tests"],
    );
    assert!(text(&first.stdout).contains("3 passed"));
    assert_eq!(e.runs().len(), 1);

    let b = e.repo("b");
    let hit = e.ok(
        &b,
        &["cache", "pytest", "--", "--junitxml=out/junit.xml", "tests"],
    );
    assert!(text(&hit.stdout).contains("3 passed"));
    assert!(b.join("out/junit.xml").is_file(), "report restored");
    assert_eq!(e.runs().len(), 1);

    // A changed test runs again; `--lf` always runs.
    std::fs::write(
        b.join("tests/test_sum.py"),
        "def test_sum():\n    assert 2 + 1 == 3\n",
    )
    .unwrap();
    e.ok(
        &b,
        &["cache", "pytest", "--", "--junitxml=out/junit.xml", "tests"],
    );
    assert_eq!(e.runs().len(), 2);
    e.ok(&b, &["cache", "pytest", "--", "--lf"]);
    e.ok(&b, &["cache", "pytest", "--", "--lf"]);
    assert_eq!(e.runs().len(), 4);

    // A failing run is not stored.
    std::fs::write(a.join("fail.txt"), "\n").unwrap();
    for _ in 0..2 {
        assert_eq!(
            e.asc(&a, "8.0.100", &["cache", "pytest", "--", "-q"])
                .status
                .code(),
            Some(1)
        );
    }
    assert_eq!(e.runs().len(), 6);
}
