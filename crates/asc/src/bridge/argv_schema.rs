//! The per-verb flag schema of `Bridge.Run` and the argv rules of SPEC §24.4.5.1 (P01b).
//!
//! The bridge, not the webview and not the host, is the authority. [`build`] turns a verb and
//! the host's argument list into the child's argument list, or refuses:
//!
//! 1. the verb is a key of [`VERBS`]; aliases, external commands and prefixes are never resolved
//!    (the child also runs with `ASC_NO_ALIASES=1`);
//! 2. an argument is a known flag of that verb (`--flag=value`, `-m=value` or a bare bool flag)
//!    or a positional of the declared type; everything in [`REJECT`] is refused outright;
//! 3. the child argv is `<verb words> --json <flags> --end-of-options <positionals>`;
//! 6. at most [`MAX_ARGS`] arguments of at most [`MAX_ARG_BYTES`] bytes.
//!
//! Rules 4 (null stdin) and 5 (rebuilt environment) belong to `run`.
//!
//! The table below must agree with `allowlist.json` (SPEC §24.4.5.1); a test checks it.
//! Known gap (P10): the test reads `packages/asc-client-ts/src/transport/allowlist.json` once P10 adds
//! it; until then it reads the JSON block of the SPEC.

use std::path::{Component, Path, PathBuf};

/// Flags refused for every verb, whatever the verb would accept (`run.reject`).
pub const REJECT: &[&str] = &[
    "-c",
    "-C",
    "--exec",
    "--exec-path",
    "--git-dir",
    "--work-tree",
    "--config-env",
    "--namespace",
    "--super-prefix",
    "--upload-pack",
    "--receive-pack",
    "--force",
    "--force-with-lease",
];
/// Most arguments of one call (`run.maxArgs`).
pub const MAX_ARGS: usize = 64;
/// Longest argument, or flag value, in bytes (`run.maxArgBytes`).
pub const MAX_ARG_BYTES: usize = 65536;

/// One verb of the schema. `flags` and `pos` use the type words of `allowlist.json`:
/// `bool`, `str:N`, `int:N`, `rev`, `path`, `tmpfile`, `user`; a positional word may end in
/// `?`, `*` or `+`, and a flag type may end in `*` (repeatable).
pub struct Verb {
    pub name: &'static str,
    pub flags: &'static [(&'static str, &'static str)],
    pub pos: &'static str,
    pub confirm: &'static str,
}

const fn v(
    name: &'static str,
    flags: &'static [(&'static str, &'static str)],
    pos: &'static str,
    confirm: &'static str,
) -> Verb {
    Verb {
        name,
        flags,
        pos,
        confirm,
    }
}

const M: (&str, &str) = ("-m", "str:65536");

/// Every verb `Bridge.Run` accepts (SPEC §24.4.5.1 `run.verbs`).
pub const VERBS: &[Verb] = &[
    v(
        "create",
        &[("--no-branch", "bool"), M, ("-a", "bool")],
        "path*",
        "",
    ),
    v(
        "modify",
        &[("--into", "rev"), M, ("-a", "bool")],
        "path*",
        "",
    ),
    v("reword", &[M], "rev?", ""),
    v("absorb", &[("-n", "bool"), ("-f", "bool")], "", ""),
    v(
        "move",
        &[("--onto", "rev"), ("--source", "rev"), ("-n", "bool")],
        "",
        "",
    ),
    v("reorder", &[("--todo", "tmpfile")], "", ""),
    v("split", &[("--hunks", "tmpfile")], "rev?", ""),
    v("squash", &[M], "rev*", ""),
    v("fold", &[M], "rev*", ""),
    v("restack", &[], "rev?", ""),
    v("sync", &[], "", ""),
    v("continue", &[], "", ""),
    v("abort", &[], "", ""),
    v("up", &[], "int:100?", ""),
    v("down", &[], "int:100?", ""),
    v("top", &[], "", ""),
    v("bottom", &[], "", ""),
    v(
        "submit",
        &[
            ("-s", "bool"),
            ("--draft", "bool"),
            ("--publish", "bool"),
            ("--reviewer", "user*"),
            ("--update-only", "bool"),
            ("--auto-merge", "bool"),
        ],
        "rev?",
        "modal",
    ),
    v("stack get", &[("--worktree", "path")], "rev", ""),
    v("stack merge", &[("--now", "bool")], "rev?", "modal"),
    v("stack delete", &[], "rev?", "modal"),
    v("stack rename", &[], "str:80", ""),
    v("stack freeze", &[], "rev?", ""),
    v("stack unfreeze", &[], "rev?", ""),
    v("hide", &[], "rev*", "modal"),
    v("unhide", &[], "rev*", ""),
    v("abandon", &[], "rev*", "modal"),
    v("uncommit", &[], "", ""),
    v("unamend", &[], "", ""),
    v("undo", &[], "", ""),
    v("redo", &[], "", ""),
    v("op log", &[("-n", "int:500")], "", ""),
    v("sl", &[("--cloud", "bool"), ("--user", "user")], "", ""),
    v("share", &[], "rev?", ""),
    v("switch", &[("--detach", "bool")], "rev", ""),
    v(
        "restore",
        &[
            ("--staged", "bool"),
            ("--worktree", "bool"),
            ("--source", "rev"),
        ],
        "path+",
        "modal",
    ),
    v("cherry-pick", &[], "rev+", ""),
    v("revert", &[], "rev+", ""),
    v("worktree add", &[("--ephemeral", "bool")], "path rev?", ""),
    v("worktree remove", &[], "path", "modal"),
    v(
        "stash push",
        &[("-u", "bool"), ("-m", "str:1024")],
        "path*",
        "",
    ),
    v("stash pop", &[], "", ""),
    v("stash apply", &[], "", ""),
    v("stash list", &[], "", ""),
    v("stash drop", &[], "", "modal"),
    v("fetch", &[("--prune", "bool")], "", ""),
    v(
        "pull",
        &[("--rebase", "bool"), ("--ff-only", "bool")],
        "",
        "",
    ),
    v("push", &[("-u", "bool")], "", "modal"),
];

/// Why a call was refused. The text is safe to show to the host: it names the rule, and never
/// echoes more than a short, control-free prefix of the offending argument.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A flag of `run.reject`, or a verb outside the schema: `PERMISSION_DENIED`.
    Denied(String),
    /// Anything else that breaks the rules: `INVALID_ARGUMENT`.
    Invalid(String),
}

fn invalid<T>(why: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal::Invalid(why.into()))
}

fn shown(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(40)
        .collect::<String>()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ty {
    Bool,
    Str(usize),
    Int(u64),
    Rev,
    Path,
    Tmpfile,
    User,
}

fn ty(word: &str) -> Option<Ty> {
    Some(match word {
        "bool" => Ty::Bool,
        "rev" => Ty::Rev,
        "path" => Ty::Path,
        "tmpfile" => Ty::Tmpfile,
        "user" => Ty::User,
        _ => {
            if let Some(n) = word.strip_prefix("str:") {
                Ty::Str(n.parse().ok()?)
            } else if let Some(n) = word.strip_prefix("int:") {
                Ty::Int(n.parse().ok()?)
            } else {
                return None;
            }
        }
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Many {
    One,
    Opt,
    Star,
    Plus,
}

/// Splits a type word and its multiplicity suffix.
fn word(w: &str) -> Option<(Ty, Many)> {
    let (body, many) = match w.as_bytes().last()? {
        b'?' => (&w[..w.len() - 1], Many::Opt),
        b'*' => (&w[..w.len() - 1], Many::Star),
        b'+' => (&w[..w.len() - 1], Many::Plus),
        _ => (w, Many::One),
    };
    Some((ty(body)?, many))
}

/// The schema entry of `name`, if any.
pub fn find(name: &str) -> Option<&'static Verb> {
    VERBS.iter().find(|v| v.name == name)
}

/// Where paths and temporary files are judged.
pub struct Scope<'a> {
    /// The workspace root the child runs in, canonical.
    pub root: &'a Path,
    /// `$ASC_PLUGIN_TMP`, canonical, when the host set one.
    pub tmp: Option<&'a Path>,
}

fn is_rev(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    s.chars().count() <= 256
        && first != '-'
        && !first.is_whitespace()
        && !first.is_control()
        && chars.all(|c| !c.is_control())
}

fn is_user(s: &str) -> bool {
    (1..=40).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Lexically normalises `p` (no filesystem access); `None` when `..` climbs above the start.
fn normalise(p: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => out.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() || out.as_os_str().is_empty() {
                    return None;
                }
            }
            Component::Normal(n) => out.push(n),
        }
    }
    Some(out)
}

/// `p` with its deepest existing ancestor canonicalised, so a symlink that leaves the root is
/// seen even when the final component does not exist yet.
fn resolve(p: &Path) -> Option<PathBuf> {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        match cur.canonicalize() {
            Ok(c) => {
                let mut c = c;
                for t in tail.iter().rev() {
                    c.push(t);
                }
                return Some(c);
            }
            Err(_) => {
                let name = cur.file_name()?.to_owned();
                tail.push(name);
                if !cur.pop() {
                    return None;
                }
            }
        }
    }
}

/// A path inside the root after canonicalisation, as the root-relative text the child gets.
fn path_in(root: &Path, s: &str) -> Result<String, Refusal> {
    if s.is_empty() || s.contains(['\0', '\n', '\r']) {
        return invalid("a path is empty or has a control character");
    }
    let given = Path::new(s);
    let joined = if given.is_absolute() {
        given.to_path_buf()
    } else {
        root.join(given)
    };
    let Some(lexical) = normalise(&joined) else {
        return invalid(format!("path '{}' leaves the workspace", shown(s)));
    };
    let left = || invalid(format!("path '{}' leaves the workspace", shown(s)));
    let Some(canonical) = resolve(&lexical) else {
        return left();
    };
    let Ok(rel) = canonical.strip_prefix(root) else {
        return left();
    };
    let mut text = String::new();
    for c in rel.components() {
        let Component::Normal(n) = c else {
            return invalid("path has an unexpected component");
        };
        if !text.is_empty() {
            text.push('/');
        }
        text.push_str(&n.to_string_lossy());
    }
    if text.is_empty() {
        text.push('.');
    }
    // Neither an option nor a pathspec magic prefix, whatever reads it.
    if text.starts_with(['-', ':']) {
        text.insert_str(0, "./");
    }
    Ok(text)
}

/// A regular file the host wrote inside `$ASC_PLUGIN_TMP`, as an absolute path.
fn tmpfile_in(tmp: Option<&Path>, s: &str) -> Result<String, Refusal> {
    let Some(tmp) = tmp else {
        return invalid("no plugin temporary directory is configured");
    };
    let p = Path::new(s);
    if s.contains('\0') || !p.is_absolute() {
        return invalid("a temporary file must be an absolute path");
    }
    let Some(lexical) = normalise(p) else {
        return invalid("a temporary file path leaves its directory");
    };
    let meta = std::fs::symlink_metadata(&lexical);
    let canon = lexical.canonicalize();
    match (meta, canon) {
        (Ok(m), Ok(c)) if m.is_file() && c.starts_with(tmp) => {
            c.to_str().map(str::to_owned).map_or_else(
                || invalid("a temporary file path is not UTF-8"),
                |t| {
                    // Windows canonical paths carry a `\\?\` prefix that tools do not expect.
                    Ok(t.strip_prefix(r"\\?\").map_or(t.clone(), str::to_owned))
                },
            )
        }
        _ => {
            invalid("a temporary file must be an existing regular file inside the plugin directory")
        }
    }
}

fn check(t: Ty, what: &str, value: &str, scope: &Scope<'_>) -> Result<String, Refusal> {
    if value.contains('\0') {
        return invalid(format!("{what} has a NUL byte"));
    }
    if value.len() > MAX_ARG_BYTES {
        return invalid(format!("{what} is longer than {MAX_ARG_BYTES} bytes"));
    }
    match t {
        Ty::Bool => invalid(format!("{what} takes no value")),
        Ty::Str(max) => {
            if value.is_empty() && max <= 1024 {
                return invalid(format!("{what} is empty"));
            }
            if value.len() > max {
                return invalid(format!("{what} is longer than {max} bytes"));
            }
            Ok(value.to_owned())
        }
        Ty::Int(max) => {
            if value.is_empty()
                || value.len() > 9
                || !value.bytes().all(|b| b.is_ascii_digit())
                || value.parse::<u64>().is_ok_and(|n| n > max)
            {
                return invalid(format!("{what} must be a number up to {max}"));
            }
            Ok(value.to_owned())
        }
        Ty::Rev => {
            if is_rev(value) {
                Ok(value.to_owned())
            } else {
                invalid(format!("{what} is not a revision"))
            }
        }
        Ty::User => {
            if is_user(value) {
                Ok(value.to_owned())
            } else {
                invalid(format!("{what} is not a user name"))
            }
        }
        Ty::Path => path_in(scope.root, value),
        Ty::Tmpfile => tmpfile_in(scope.tmp, value),
    }
}

/// The child's arguments (after the program name) for `verb` and the host's `args`.
///
/// # Errors
/// [`Refusal::Denied`] for a verb outside the schema or a flag of [`REJECT`];
/// [`Refusal::Invalid`] for any other rule.
pub fn build(verb: &str, args: &[String], scope: &Scope<'_>) -> Result<Vec<String>, Refusal> {
    let Some(entry) = find(verb) else {
        return Err(Refusal::Denied(format!(
            "verb '{}' is not allowed",
            shown(verb)
        )));
    };
    if args.len() > MAX_ARGS {
        return invalid(format!("more than {MAX_ARGS} arguments"));
    }
    let mut flags: Vec<String> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    let mut positional: Vec<&str> = Vec::new();
    for arg in args {
        if arg.len() > MAX_ARG_BYTES + 8 {
            return invalid(format!("an argument is longer than {MAX_ARG_BYTES} bytes"));
        }
        if arg.contains('\0') {
            return invalid("an argument has a NUL byte");
        }
        if !arg.starts_with('-') {
            positional.push(arg);
            continue;
        }
        let (name, value) = match arg.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (arg.as_str(), None),
        };
        if REJECT.contains(&name) {
            return Err(Refusal::Denied(format!(
                "flag '{}' is not allowed",
                shown(name)
            )));
        }
        let Some((_, spec)) = entry.flags.iter().find(|(f, _)| *f == name) else {
            return invalid(format!(
                "unknown flag '{}' for '{}'",
                shown(name),
                entry.name
            ));
        };
        let (body, repeat) = spec.strip_suffix('*').map_or((*spec, false), |b| (b, true));
        let Some(t) = ty(body) else {
            return invalid("the flag schema is malformed");
        };
        if !repeat && seen.contains(&name) {
            return invalid(format!("flag '{}' is given twice", shown(name)));
        }
        seen.push(name);
        match (t, value) {
            (Ty::Bool, None) => flags.push(name.to_owned()),
            (Ty::Bool, Some(_)) => {
                return invalid(format!("flag '{}' takes no value", shown(name)));
            }
            (_, None) => {
                return invalid(format!("flag '{}' needs '{name}=value'", shown(name)));
            }
            (t, Some(v)) => {
                let v = check(t, &format!("the value of '{}'", shown(name)), v, scope)?;
                flags.push(format!("{}={v}", long_form(name)));
            }
        }
    }
    let pos = positionals(entry, &positional, scope)?;
    let mut out: Vec<String> = entry.name.split(' ').map(str::to_owned).collect();
    out.push("--json".to_owned());
    out.extend(flags);
    if !pos.is_empty() {
        out.push("--end-of-options".to_owned());
        out.extend(pos);
    }
    Ok(out)
}

/// The form of a flag the child is given. A short flag with a value (`-m=text`) reads as a
/// cluster of short flags to the native router, so the one short flag that takes a value here
/// goes as its long name.
fn long_form(name: &str) -> &str {
    match name {
        "-m" => "--message",
        other => other,
    }
}

fn positionals(entry: &Verb, given: &[&str], scope: &Scope<'_>) -> Result<Vec<String>, Refusal> {
    let mut out = Vec::new();
    let mut next = 0usize;
    for w in entry.pos.split(' ').filter(|w| !w.is_empty()) {
        let Some((t, many)) = word(w) else {
            return invalid("the positional schema is malformed");
        };
        let left = given.len() - next;
        let take = match many {
            Many::One => {
                if left == 0 {
                    return invalid(format!("'{}' needs another argument", entry.name));
                }
                1
            }
            Many::Opt => left.min(1),
            Many::Star => left,
            Many::Plus => {
                if left == 0 {
                    return invalid(format!("'{}' needs at least one argument", entry.name));
                }
                left
            }
        };
        for g in &given[next..next + take] {
            out.push(check(t, "an argument", g, scope)?);
        }
        next += take;
    }
    if next < given.len() {
        return invalid(format!("'{}' takes fewer arguments", entry.name));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn scope<'a>(root: &'a Path, tmp: Option<&'a Path>) -> Scope<'a> {
        Scope { root, tmp }
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    fn root() -> PathBuf {
        let d = std::env::temp_dir().join(format!("asc-argv-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    #[test]
    fn builds_the_child_argv_with_end_of_options_before_positionals() {
        let r = root();
        let got = build(
            "modify",
            &args(&["--into=abc", "-m=hello", "-a", "src/a.rs"]),
            &scope(&r, None),
        )
        .unwrap();
        assert_eq!(
            got,
            [
                "modify",
                "--json",
                "--into=abc",
                "--message=hello",
                "-a",
                "--end-of-options",
                "src/a.rs"
            ]
        );
        let two = build("stack get", &args(&["main"]), &scope(&r, None)).unwrap();
        assert_eq!(two, ["stack", "get", "--json", "--end-of-options", "main"]);
        assert_eq!(
            build("sync", &[], &scope(&r, None)).unwrap(),
            ["sync", "--json"]
        );
    }

    #[test]
    fn rejects_the_injection_corpus() {
        let r = root();
        let s = scope(&r, None);
        for bad in [
            "-c",
            "-C",
            "--exec",
            "--exec=rm",
            "--exec-path=/x",
            "--git-dir=/x",
            "--upload-pack=x",
            "--force",
            "--force-with-lease=x",
            "--",
            "--end-of-options",
            "--json",
            "-x",
            "-cfoo=bar",
            "--no-branch=1",
            "-m",
            "-",
        ] {
            assert!(build("create", &args(&[bad]), &s).is_err(), "{bad}");
        }
        // A flag the verb does not know, and a flag from another verb.
        assert!(build("sync", &args(&["--prune"]), &s).is_err());
        // The same flag twice.
        assert!(build("create", &args(&["-a", "-a"]), &s).is_err());
        // Verbs outside the schema, aliases and prefixes.
        for verb in [
            "",
            "git",
            "log",
            "cre",
            "creat",
            "create ",
            "Create",
            "alias",
            "!sh",
            "stack",
            "stack  get",
            "create --json",
            "create\0",
        ] {
            assert!(
                matches!(build(verb, &[], &s), Err(Refusal::Denied(_))),
                "{verb:?}"
            );
        }
        // Values that start with '-' never reach the child as positionals.
        assert!(build("switch", &args(&["-x"]), &s).is_err());
        assert!(build("switch", &args(&["--", "-x"]), &s).is_err());
        assert!(build("switch", &args(&["--detach=1", "a"]), &s).is_err());
        assert!(build("switch", &args(&["--source=a", "a"]), &s).is_err());
        assert!(build("move", &args(&["--onto=-x"]), &s).is_err());
        assert!(build("move", &args(&["--onto"]), &s).is_err());
        // Reject wins over a flag a verb would accept.
        assert!(matches!(
            build("push", &args(&["--force"]), &s),
            Err(Refusal::Denied(_))
        ));
    }

    #[test]
    fn enforces_types_and_limits() {
        let r = root();
        let s = scope(&r, None);
        assert!(build("up", &args(&["100"]), &s).is_ok());
        assert!(build("up", &args(&["101"]), &s).is_err());
        assert!(build("up", &args(&["1", "2"]), &s).is_err());
        assert!(build("up", &args(&["x"]), &s).is_err());
        assert!(build("op log", &args(&["-n=500"]), &s).is_ok());
        assert!(build("op log", &args(&["-n=501"]), &s).is_err());
        assert!(build("sl", &args(&["--user=a b"]), &s).is_err());
        assert!(build("sl", &args(&["--user=a.b_c-d"]), &s).is_ok());
        assert!(build("submit", &args(&["--reviewer=a", "--reviewer=b"]), &s).is_ok());
        assert!(build("stack rename", &args(&[&"x".repeat(81)]), &s).is_err());
        assert!(build("stack rename", &args(&["ok"]), &s).is_ok());
        assert!(build("switch", &[], &s).is_err());
        assert!(build("cherry-pick", &[], &s).is_err());
        assert!(build("reword", &args(&["a", "b"]), &s).is_err());
        let msg = "x".repeat(65536);
        assert!(build("reword", &args(&[&format!("-m={msg}")]), &s).is_ok());
        assert!(build("reword", &args(&[&format!("-m={msg}x")]), &s).is_err());
        assert!(
            build(
                "stash push",
                &args(&[&format!("-m={}", "y".repeat(1025))]),
                &s
            )
            .is_err()
        );
        assert!(build("reword", &args(&["-m=a\0b"]), &s).is_err());
        let many: Vec<String> = (0..65).map(|i| format!("r{i}")).collect();
        assert!(build("hide", &many, &s).is_err());
        assert!(build("hide", &many[..64], &s).is_ok());
        // A revision with a space inside is fine; one that starts with one is not.
        assert!(build("switch", &args(&["a b"]), &s).is_ok());
        assert!(build("switch", &args(&[" a"]), &s).is_err());
        assert!(build("switch", &args(&["a\nb"]), &s).is_err());
        assert!(build("switch", &args(&[""]), &s).is_err());
    }

    #[test]
    fn paths_stay_inside_the_root() {
        let r = root();
        let s = scope(&r, None);
        std::fs::create_dir_all(r.join("src")).unwrap();
        let ok = |p: &str| build("create", &args(&[p]), &s).map(|v| v.last().cloned().unwrap());
        assert_eq!(ok("src/a.rs").unwrap(), "src/a.rs");
        assert_eq!(ok("./src/../src/a.rs").unwrap(), "src/a.rs");
        assert_eq!(ok("src").unwrap(), "src");
        assert_eq!(ok(":(top)x").unwrap(), "./:(top)x");
        assert!(ok("../x").is_err());
        assert!(ok("src/../../x").is_err());
        let outside = std::env::temp_dir().join("asc-argv-outside-file");
        assert!(ok(outside.to_str().unwrap()).is_err());
        let inside_abs = r.join("src").join("b.rs");
        assert_eq!(ok(inside_abs.to_str().unwrap()).unwrap(), "src/b.rs");
        assert!(ok("").is_err());
    }

    #[test]
    fn tmpfiles_come_from_the_plugin_directory_only() {
        let r = root();
        let t = r.join("plugin-tmp");
        std::fs::create_dir_all(&t).unwrap();
        let t = t.canonicalize().unwrap();
        let f = t.join("todo.json");
        std::fs::write(&f, b"{}").unwrap();
        let other = r.join("other.json");
        std::fs::write(&other, b"{}").unwrap();
        let s = scope(&r, Some(&t));
        let ok = build("reorder", &args(&[&format!("--todo={}", f.display())]), &s).unwrap();
        assert_eq!(ok[0], "reorder");
        for bad in [
            format!("--todo={}", other.display()),
            "--todo=relative.json".to_owned(),
            format!("--todo={}", t.display()),
            format!("--todo={}", t.join("missing").display()),
            format!("--todo={}", t.join("..").join("other.json").display()),
        ] {
            assert!(build("reorder", &args(&[&bad]), &s).is_err(), "{bad}");
        }
        assert!(
            build(
                "reorder",
                &args(&[&format!("--todo={}", f.display())]),
                &scope(&r, None)
            )
            .is_err()
        );
    }

    /// The allowlist as checked in, else the JSON block of the SPEC.
    fn allowlist() -> Value {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let file = base.join("packages/asc-client-ts/src/transport/allowlist.json");
        if let Ok(text) = std::fs::read_to_string(&file) {
            return serde_json::from_str(&text).unwrap();
        }
        let spec = std::fs::read_to_string(base.join("docs/SPEC.md")).unwrap();
        let at = spec
            .find("##### 24.4.5.1")
            .expect("the SPEC has section 24.4.5.1");
        let rest = &spec[at..];
        let start = rest.find("```json\n").unwrap() + "```json\n".len();
        let end = start + rest[start..].find("\n```").unwrap();
        serde_json::from_str(&rest[start..end]).unwrap()
    }

    #[test]
    fn the_schema_agrees_with_the_allowlist() {
        let list = allowlist();
        let run = &list["run"];
        let reject: Vec<&str> = run["reject"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(reject, REJECT);
        assert_eq!(run["maxArgs"], MAX_ARGS);
        assert_eq!(run["maxArgBytes"], MAX_ARG_BYTES);
        let verbs = run["verbs"].as_object().unwrap();
        assert_eq!(verbs.len(), VERBS.len(), "verb count");
        for (name, spec) in verbs {
            let ours = find(name).unwrap_or_else(|| panic!("verb '{name}' missing"));
            let flags = spec["flags"].as_object().cloned().unwrap_or_default();
            assert_eq!(flags.len(), ours.flags.len(), "{name}: flag count");
            for (f, t) in &flags {
                let mine = ours.flags.iter().find(|(n, _)| n == f);
                assert_eq!(mine.map(|(_, t)| *t), t.as_str(), "{name} {f}");
            }
            assert_eq!(spec["pos"].as_str().unwrap_or(""), ours.pos, "{name}: pos");
            assert_eq!(
                spec["confirm"].as_str().unwrap_or(""),
                ours.confirm,
                "{name}: confirm"
            );
        }
        for v in VERBS {
            for (_, t) in v.flags {
                assert!(ty(t.trim_end_matches('*')).is_some(), "{}: {t}", v.name);
            }
            for w in v.pos.split(' ').filter(|w| !w.is_empty()) {
                assert!(word(w).is_some(), "{}: {w}", v.name);
            }
        }
    }
}
