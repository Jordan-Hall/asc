//! `asc cache setup` for the tools the cache fronts answer (docs/designs/build-cache.md §4.3):
//! each tool's user configuration names the front's port and token, so nothing of it is
//! committed. What setup writes is marked, and `--undo` takes out only that.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::{Change, SetupArgs, Tool, print};
use crate::cli::cache::front::FrontFile;
use crate::cli::cache::front_file;
use crate::cli::common::{CliError, Ctx, Result};

/// The id of the Maven server entry that holds the token.
pub(crate) const MAVEN_SERVER: &str = "asc-build-cache";

/// Markers around what setup writes in a file it shares with the user.
struct Marks {
    begin: &'static str,
    end: &'static str,
}

const HASH: Marks = Marks {
    begin: "# asc cache setup {",
    end: "# asc cache setup }",
};
const XML: Marks = Marks {
    begin: "<!-- asc cache setup { -->",
    end: "<!-- asc cache setup } -->",
};

/// `text` without the marked block, and whether there was one.
fn remove_block(text: &str, m: &Marks) -> (String, bool) {
    let (Some(b), Some(e)) = (text.find(m.begin), text.find(m.end)) else {
        return (text.to_owned(), false);
    };
    if e < b {
        return (text.to_owned(), false);
    }
    // The indent of the begin marker goes with it.
    let b = text[..b].trim_end_matches([' ', '\t']).len();
    let mut end = e + m.end.len();
    if text[end..].starts_with("\r\n") {
        end += 2;
    } else if text[end..].starts_with('\n') {
        end += 1;
    }
    (format!("{}{}", &text[..b], &text[end..]), true)
}

/// `body` wrapped in the markers.
fn block(body: &str, m: &Marks, indent: &str) -> String {
    format!("{indent}{}\n{body}{indent}{}\n", m.begin, m.end)
}

/// `text` with the marked block set to `body`: replaced in place, else added at the end.
fn set_block(text: &str, body: &str, m: &Marks) -> String {
    if let (Some(b), Some(_)) = (text.find(m.begin), text.find(m.end)) {
        let (rest, _) = remove_block(text, m);
        let mut out = rest;
        out.insert_str(b, &block(body, m, ""));
        return out;
    }
    let mut out = text.to_owned();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&block(body, m, ""));
    out
}

/// The start of the line holding byte `i`, when only blanks come before it on that line.
fn line_start(text: &str, i: usize) -> usize {
    let head = text[..i].trim_end_matches([' ', '\t']);
    if head.is_empty() || head.ends_with('\n') {
        head.len()
    } else {
        i
    }
}

fn read(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e.into()),
    }
}

/// Writes `text` to `path` through a sibling temporary file and a rename, so a reader never sees
/// half a file. The text holds the front's token, so the file is owner-only on Unix: `0600`, or
/// the existing mode when it is already stricter. A file reached by a link (a dotfiles checkout)
/// is written at its target, and the link stays.
fn write(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let path = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
        Err(e) => return Err(e.into()),
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.asc-{}.tmp", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mode = match std::fs::metadata(&path) {
            Ok(m) => m.permissions().mode() & 0o600,
            Err(_) => 0o600,
        };
        opts.mode(mode);
    }
    let res = opts
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(text.as_bytes())?;
            f.sync_all()
        })
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

fn shown(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

fn home() -> Result<PathBuf> {
    std::env::home_dir().ok_or_else(|| CliError::Fatal("cannot find the home directory".into()))
}

/// Applies a marked block to the file at `path` (or takes it out) and prints what changed.
fn edit_marked(
    path: &Path,
    body: &str,
    m: &Marks,
    name: &'static str,
    undo: bool,
    out: &mut impl Write,
) -> Result<()> {
    let text = read(path)?;
    let (next, change) = if undo {
        let (rest, had) = remove_block(&text, m);
        (rest, had.then_some(Change::Removed(name)))
    } else {
        let next = set_block(&text, body, m);
        let change = (next != text).then(|| Change::Set(name, "(asc block)".into()));
        (next, change)
    };
    let changes: Vec<Change> = change.into_iter().collect();
    if !changes.is_empty() {
        if undo && next.trim().is_empty() {
            let _ = std::fs::remove_file(path);
        } else {
            write(path, &next)?;
        }
    }
    print(out, &shown(path), &changes)
}

/// Bazel's HTTP cache in the user's `.bazelrc`.
fn bazel_rc(f: &FrontFile) -> String {
    format!(
        "build --remote_cache={}/bazel\nbuild --remote_header=Authorization=Bearer {}\n",
        f.url(),
        f.token
    )
}

/// Gradle's init script: it reads the front file when each build starts, and does nothing when
/// the file is gone.
fn gradle_init(front: &Path) -> String {
    let path = shown(front).replace('\'', "\\'");
    format!(
        "// Written by `asc cache setup gradle`; `asc cache setup gradle --undo` removes it.\n\
         def ascFront = new File('{path}')\n\
         if (ascFront.isFile()) {{\n\
         \x20   def f = new groovy.json.JsonSlurper().parse(ascFront)\n\
         \x20   gradle.settingsEvaluated {{ settings ->\n\
         \x20       settings.buildCache {{\n\
         \x20           remote(HttpBuildCache) {{\n\
         \x20               url = \"http://127.0.0.1:${{f.port}}/gradle/\"\n\
         \x20               allowInsecureProtocol = true\n\
         \x20               push = true\n\
         \x20               credentials {{\n\
         \x20                   username = 'asc'\n\
         \x20                   password = f.token\n\
         \x20               }}\n\
         \x20           }}\n\
         \x20       }}\n\
         \x20   }}\n\
         }}\n"
    )
}

fn gradle_home() -> Result<PathBuf> {
    match std::env::var_os("GRADLE_USER_HOME") {
        Some(h) if !h.is_empty() => Ok(PathBuf::from(h)),
        _ => Ok(home()?.join(".gradle")),
    }
}

fn gradle(f: &FrontFile, front: &Path, undo: bool, out: &mut impl Write) -> Result<()> {
    let dir = gradle_home()?;
    let script = dir.join("init.d").join("asc-build-cache.gradle");
    let body = gradle_init(front);
    if undo {
        let had = script.is_file();
        if had {
            std::fs::remove_file(&script)?;
        }
        let changes: Vec<Change> = had
            .then_some(Change::Removed("init script"))
            .into_iter()
            .collect();
        print(out, &shown(&script), &changes)?;
    } else {
        let same = read(&script)? == body;
        if !same {
            write(&script, &body)?;
        }
        let changes: Vec<Change> = (!same)
            .then(|| Change::Set("init script", format!("remote cache {}/gradle/", f.url())))
            .into_iter()
            .collect();
        print(out, &shown(&script), &changes)?;
    }
    // The init script names the remote cache; caching itself is `org.gradle.caching`.
    edit_marked(
        &dir.join("gradle.properties"),
        "org.gradle.caching=true\n",
        &HASH,
        "org.gradle.caching",
        undo,
        out,
    )
}

/// The `<server>` entry of the user's Maven settings that holds the token.
fn maven(f: &FrontFile, undo: bool, out: &mut impl Write) -> Result<()> {
    let path = home()?.join(".m2").join("settings.xml");
    let text = read(&path)?;
    let server = format!(
        "    <server>\n      <id>{MAVEN_SERVER}</id>\n      <username>asc</username>\n      <password>{}</password>\n    </server>\n",
        f.token
    );
    let (rest, had) = remove_block(&text, &XML);
    let next = if undo {
        rest.clone()
    } else if rest.trim().is_empty() {
        format!(
            "<settings>\n  <servers>\n{}  </servers>\n</settings>\n",
            block(&server, &XML, "    ")
        )
    } else if let Some(i) = rest.find("</servers>") {
        let mut t = rest.clone();
        t.insert_str(line_start(&rest, i), &block(&server, &XML, "    "));
        t
    } else if let Some(i) = rest.rfind("</settings>") {
        let mut t = rest.clone();
        t.insert_str(
            line_start(&rest, i),
            &format!(
                "  <servers>\n{}  </servers>\n",
                block(&server, &XML, "    ")
            ),
        );
        t
    } else {
        return Err(CliError::Fatal(format!(
            "{}: not a Maven settings file (no </settings>)",
            shown(&path)
        )));
    };
    let changes: Vec<Change> = if next == text {
        Vec::new()
    } else if undo {
        had.then_some(Change::Removed("server asc-build-cache"))
            .into_iter()
            .collect()
    } else {
        vec![Change::Set("server", MAVEN_SERVER.into())]
    };
    if !changes.is_empty() {
        write(&path, &next)?;
    }
    print(out, &shown(&path), &changes)?;
    if !undo {
        writeln!(
            out,
            "note: the project needs the Maven build cache extension; `asc cache env` gives \
             MAVEN_ARGS that point it at the front"
        )?;
    }
    Ok(())
}

/// ccache's `remote_storage`, through `ccache --set-config`.
fn ccache(f: &FrontFile, undo: bool, out: &mut impl Write) -> Result<()> {
    let run = |args: &[&str]| -> Result<String> {
        let o = std::process::Command::new(crate::cli::cache::run::program(&"ccache".into()))
            .args(args)
            .output()
            .map_err(|e| CliError::Fatal(format!("cannot run 'ccache': {e}")))?;
        if !o.status.success() {
            return Err(CliError::Fatal(format!(
                "ccache {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&o.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    let wanted = format!("http://asc:{}@127.0.0.1:{}/ccache", f.token, f.port);
    let current = run(&["--get-config", "remote_storage"])?;
    let ours = current.contains("@127.0.0.1:") && current.contains("/ccache");
    let change = if undo {
        ours.then_some(("", Change::Removed("remote_storage")))
    } else if current == wanted {
        None
    } else if !current.is_empty() && !ours {
        return Err(CliError::Fatal(format!(
            "ccache's remote_storage is already '{current}'; remove it first"
        )));
    } else {
        Some((
            wanted.as_str(),
            Change::Set("remote_storage", format!("{}/ccache", f.url())),
        ))
    };
    let changes: Vec<Change> = match change {
        Some((value, c)) => {
            run(&["--set-config", &format!("remote_storage={value}")])?;
            vec![c]
        }
        None => Vec::new(),
    };
    print(out, "ccache.conf", &changes)
}

pub(super) fn run(ctx: &Ctx, a: &SetupArgs, out: &mut impl Write) -> Result<ExitCode> {
    if a.repo {
        return Err(CliError::Usage(
            "error: the cache front's port and token are this machine's, so setup writes the \
             user's configuration; leave out --repo"
                .into(),
        ));
    }
    let f = front_file(ctx)?;
    let front = FrontFile::path(&asc_state::AscHome::locate()?);
    match a.tool {
        Tool::Bazel => edit_marked(
            &home()?.join(".bazelrc"),
            &bazel_rc(&f),
            &HASH,
            "build --remote_cache",
            a.undo,
            out,
        )?,
        Tool::Gradle => gradle(&f, &front, a.undo, out)?,
        Tool::Maven => maven(&f, a.undo, out)?,
        Tool::Ccache => ccache(&f, a.undo, out)?,
        Tool::Turbo | Tool::Nx => {
            if !a.undo {
                writeln!(
                    out,
                    "{}: front on {}\n{} reads its remote cache from the environment: run \
                     `asc cache env` in the shell (the VS Code extension sets it in its \
                     terminals and tasks)",
                    shown(&front),
                    f.url(),
                    if a.tool == Tool::Turbo { "turbo" } else { "Nx" }
                )?;
            } else {
                writeln!(out, "{}: nothing to change", shown(&front))?;
            }
        }
        Tool::Cargo | Tool::Go => unreachable!("not a front"),
    }
    if !a.undo {
        writeln!(
            out,
            "the daemon serves the front on {}; without it, run `asc cache serve`",
            f.url()
        )?;
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marked_block_is_set_in_place_and_removed_alone() {
        let start = "build --jobs=4\n";
        let once = set_block(start, "a\n", &HASH);
        assert_eq!(
            once,
            "build --jobs=4\n# asc cache setup {\na\n# asc cache setup }\n"
        );
        let twice = set_block(&once, "b\n", &HASH);
        assert_eq!(
            twice,
            "build --jobs=4\n# asc cache setup {\nb\n# asc cache setup }\n"
        );
        assert_eq!(remove_block(&twice, &HASH), (start.to_owned(), true));
        assert_eq!(remove_block(start, &HASH), (start.to_owned(), false));
        let tail = "x\n# asc cache setup {\nb\n# asc cache setup }\ny\n";
        assert_eq!(remove_block(tail, &HASH).0, "x\ny\n");
    }

    #[test]
    fn an_indented_block_is_removed_with_its_indent() {
        let start = "<servers>\n  </servers>\n";
        let i = line_start(start, start.find("</servers>").unwrap());
        let mut t = start.to_owned();
        t.insert_str(i, &block("      <server/>\n", &XML, "    "));
        assert_eq!(
            t,
            "<servers>\n    <!-- asc cache setup { -->\n      <server/>\n    <!-- asc cache setup } -->\n  </servers>\n"
        );
        assert_eq!(remove_block(&t, &XML), (start.to_owned(), true));
    }

    #[test]
    fn the_gradle_script_reads_the_front_file() {
        let s = gradle_init(Path::new("C:\\Users\\a\\.asc\\buildcache-front.json"));
        assert!(
            s.contains("new File('C:/Users/a/.asc/buildcache-front.json')"),
            "{s}"
        );
        assert!(
            s.contains("url = \"http://127.0.0.1:${f.port}/gradle/\""),
            "{s}"
        );
        assert!(s.contains("allowInsecureProtocol = true"));
    }

    #[test]
    fn a_write_replaces_the_file_whole_and_leaves_no_temporary() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub/.bazelrc");
        write(
            &p, "one
",
        )
        .unwrap();
        write(
            &p, "two
",
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "two
"
        );
        assert_eq!(std::fs::read_dir(d.path().join("sub")).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_written_file_is_owner_only_and_a_link_stays_a_link() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("dotfiles-bazelrc");
        std::fs::write(
            &target, "mine
",
        )
        .unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        let link = d.path().join(".bazelrc");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        write(
            &link, "token
",
        )
        .unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "token
"
        );
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o400)).unwrap();
        write(
            &link, "again
",
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o400
        );
        let fresh = d.path().join("new.xml");
        write(&fresh, "x").unwrap();
        assert_eq!(
            std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
