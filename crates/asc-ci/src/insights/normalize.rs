//! Failure message normalisation and the failure signature (docs/BUILDOUT.md section 8).
//!
//! A test runner prints the same failure differently from run to run: line numbers shift, temp
//! directories, ports, ids, timestamps and durations change. [`normalise`] removes those so two
//! occurrences of one failure produce one text, and [`signature`] hashes the text with the job and
//! the test name. Failures with one signature form one group.
//!
//! The location of a failure is the first source position the message names (a Rust panic, a Go
//! `file_test.go:12:` line, a Python `File "x", line 3`, a JavaScript or Java stack frame, or any
//! `path.ext:line` token). It is kept as a path without line and column, so an edit above the
//! failing line does not start a new group. An absolute path is cut to the part that starts at a
//! well known source directory (`src`, `tests`, `crates`, ...), else to its last two segments, so
//! two runners with different checkout roots agree.

use std::sync::LazyLock;

use regex::Regex;

/// Longest message the normaliser reads. Longer input is cut on a character boundary.
pub const MAX_MESSAGE: usize = 8 * 1024;
/// Longest title, in characters.
pub const MAX_TITLE: usize = 200;
/// Lines of the message that count towards the signature, after noise lines are dropped.
const SIGNATURE_LINES: usize = 3;

/// A normalised failure message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Normalised {
    /// The first source position the message names, without line and column.
    pub location: String,
    /// The first meaningful line, normalised and cut to [`MAX_TITLE`] characters.
    pub title: String,
    /// The first meaningful lines, normalised, one per line. It starts with [`Normalised::title`].
    pub body: String,
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a fixed pattern compiles")
}

static ANSI: LazyLock<Regex> = LazyLock::new(|| re(r"\x1b\[[0-9;?]*[ -/]*[@-~]"));
static UUID: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b"));
static TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"\b\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?|\b\d{2}:\d{2}:\d{2}(?:[.,]\d+)?\b",
    )
});
static ENDPOINT: LazyLock<Regex> = LazyLock::new(|| {
    re(r"\b\d{1,3}(?:\.\d{1,3}){3}(?::\d{1,5})?\b|\b(?:localhost|\[[0-9a-fA-F:]+\]):\d{1,5}\b")
});
static WIN_PATH: LazyLock<Regex> =
    LazyLock::new(|| re(r#"\b[A-Za-z]:\\(?:[^\\/:*?"<>|\s]+\\)*[^\\/:*?"<>|\s]*"#));
static UNIX_PATH: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?:^|[\s'\x22(\[=])(/(?:[\w.@+~-]+/)+[\w.@+~-]*)"));
static HEX: LazyLock<Regex> = LazyLock::new(|| re(r"\b0x[0-9a-fA-F]+\b|\b[0-9a-fA-F]{8,}\b"));
static DURATION: LazyLock<Regex> =
    LazyLock::new(|| re(r"\b\d+(?:\.\d+)?\s?(?:ns|us|µs|ms|s|sec|secs|seconds|m|min|h)\b"));
static NUMBER: LazyLock<Regex> = LazyLock::new(|| re(r"\d+(?:\.\d+)?"));
static SPACES: LazyLock<Regex> = LazyLock::new(|| re(r"[ \t]+"));

/// `thread 'name' panicked at path:line:col:` (Rust 1.73 and later).
static RUST_PANIC: LazyLock<Regex> =
    LazyLock::new(|| re(r"panicked at ([^\s:'\x22][^\n]*?):(\d+)(?::(\d+))?:?\s*$"));
/// `panicked at 'message', path:line:col` (older Rust).
static RUST_PANIC_OLD: LazyLock<Regex> =
    LazyLock::new(|| re(r"panicked at .*?,\s+([^\s,]+\.\w+):(\d+)(?::(\d+))?\s*$"));
/// `File "path", line N` (Python).
static PY_FRAME: LazyLock<Regex> = LazyLock::new(|| re(r#"File "([^"]+)", line (\d+)"#));
/// `at fn (path:line:col)` and `at path:line:col` (JavaScript).
static JS_FRAME: LazyLock<Regex> =
    LazyLock::new(|| re(r"\bat (?:[^(\n]*\()?([^\s()]+\.\w+):(\d+)(?::(\d+))?\)?"));
/// `at pkg.Class.method(File.java:123)` (Java and Kotlin).
static JVM_FRAME: LazyLock<Regex> =
    LazyLock::new(|| re(r"\bat [\w.$<>]+\(([\w$]+\.(?:java|kt|scala|groovy)):(\d+)\)"));
/// Any `path.ext:line` token at the start of a line or after a space (Go, C, TypeScript).
static PATH_LINE: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?m)(?:^|[\s(])((?:[A-Za-z]:)?[\w./\\@+-]+\.[A-Za-z][\w]{0,5}):(\d+)(?::(\d+))?")
});

/// Directories that hold the packages of a workspace; an absolute path is cut at the outermost.
const WORKSPACE_MARKERS: &[&str] = &["crates", "packages", "internal", "cmd", "pkg"];
/// Source directories at which an absolute path is cut when it names no workspace directory.
const ROOT_MARKERS: &[&str] = &["src", "tests", "test", "lib", "app", "spec"];

/// Cuts `s` to at most `max` bytes on a character boundary.
fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Whether `path` is absolute on Unix or Windows.
fn is_absolute(path: &str) -> bool {
    path.starts_with('/')
        || path.starts_with('\\')
        || (path.len() > 2 && path.as_bytes()[0].is_ascii_alphabetic() && &path[1..3] == ":\\")
        || (path.len() > 2 && path.as_bytes()[0].is_ascii_alphabetic() && &path[1..3] == ":/")
}

/// The stable form of a source path: forward slashes, no `./`, absolute roots removed.
pub fn normalise_path(path: &str) -> String {
    let slashed = path.replace('\\', "/");
    let segments: Vec<&str> = slashed
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    if !is_absolute(path) {
        let rel: Vec<&str> = segments.iter().copied().filter(|s| *s != "..").collect();
        return rel.join("/");
    }
    // A workspace-level directory (`crates`, `packages`, ...) wins, outermost first. Else the
    // innermost general marker that still leaves a file after it (a checkout root can itself be
    // called `src`, as in `/src/app/src/lib.rs`).
    let leaves_file = |i: usize| i + 1 < segments.len();
    let cut_at = segments
        .iter()
        .enumerate()
        .find(|(i, s)| WORKSPACE_MARKERS.contains(s) && leaves_file(*i))
        .or_else(|| {
            segments
                .iter()
                .enumerate()
                .rev()
                .find(|(i, s)| ROOT_MARKERS.contains(s) && leaves_file(*i))
        })
        .map(|(i, _)| i);
    let tail: &[&str] = match cut_at {
        Some(i) => &segments[i..],
        None => &segments[segments.len().saturating_sub(2)..],
    };
    tail.join("/")
}

/// The first source position in `text`, as a normalised path, and the line that names it.
fn find_location(text: &str) -> Option<(String, usize)> {
    // One pass per pattern, in order of how specific the pattern is. The earliest match in the
    // text wins inside a pattern; a more specific pattern wins over a later, looser one.
    let patterns: [&LazyLock<Regex>; 6] = [
        &RUST_PANIC,
        &RUST_PANIC_OLD,
        &PY_FRAME,
        &JVM_FRAME,
        &JS_FRAME,
        &PATH_LINE,
    ];
    for (index, pattern) in patterns.iter().enumerate() {
        let hit = if index == 2 {
            // Python prints the innermost frame last, and that is where the assertion is.
            pattern.captures_iter(text).last()
        } else {
            pattern.captures_iter(text).next()
        };
        if let Some(caps) = hit {
            let whole = caps.get(0).expect("group 0");
            let path = caps.get(1).expect("group 1").as_str();
            let line_no = text[..whole.start()].matches('\n').count();
            let normal = normalise_path(path);
            if !normal.is_empty() {
                return Some((normal, line_no));
            }
        }
    }
    None
}

/// Replaces the parts of a line that change between runs.
fn scrub(line: &str) -> String {
    let s = ANSI.replace_all(line, "");
    let s = UUID.replace_all(&s, "<uuid>");
    let s = TIMESTAMP.replace_all(&s, "<ts>");
    let s = ENDPOINT.replace_all(&s, "<addr>");
    let s = WIN_PATH.replace_all(&s, "<path>");
    let s = UNIX_PATH.replace_all(&s, |c: &regex::Captures<'_>| {
        let whole = c.get(0).expect("group 0").as_str();
        let path = c.get(1).expect("group 1").as_str();
        // Keep the character before the path (a space, a quote, a bracket).
        let lead = &whole[..whole.len() - path.len()];
        format!("{lead}<path>")
    });
    let s = HEX.replace_all(&s, "<hex>");
    let s = DURATION.replace_all(&s, "<dur>");
    let s = NUMBER.replace_all(&s, "<n>");
    SPACES.replace_all(s.trim(), " ").into_owned()
}

/// Whether a line is noise that differs between runs of the same failure and says nothing about
/// the cause: backtrace frames, runner hints and separators.
fn is_noise(raw: &str) -> bool {
    let t = raw.trim();
    if t.is_empty() {
        return true;
    }
    let lower = t.to_ascii_lowercase();
    lower.starts_with("note: run with")
        || lower.starts_with("stack backtrace")
        || lower.starts_with("traceback (most recent call last)")
        || lower.starts_with("---- ")
        || lower.starts_with("====")
        || lower.starts_with("----")
        || (t.starts_with("at ") && (t.contains('(') || PATH_LINE.is_match(t)))
        || t.starts_with("File \"")
        || t.chars().all(|c| matches!(c, '-' | '=' | '_' | '*' | '^' | '~' | '.'))
        // `   12: core::panicking::panic` and `  at /rustc/...` frames.
        || (t.split_once(':').is_some_and(|(n, _)| !n.is_empty() && n.trim().bytes().all(|b| b.is_ascii_digit())))
}

/// Removes the `thread 'x' panicked at ...` header or reduces a line to its message.
fn strip_header(line: &str) -> Option<String> {
    let t = line.trim();
    if t.starts_with("thread '") && t.contains("panicked at") {
        // New format: the message follows on the next lines. Old format: `panicked at 'msg', loc`.
        if let Some(rest) = t.split_once("panicked at ").map(|(_, r)| r)
            && let Some(inner) = rest.strip_prefix('\'')
            && let Some((msg, _)) = inner.rsplit_once("',")
        {
            return Some(msg.to_owned());
        }
        return None;
    }
    Some(t.to_owned())
}

/// Normalises a failure message.
pub fn normalise(message: &str) -> Normalised {
    let message = cut(message, MAX_MESSAGE);
    let plain = ANSI
        .replace_all(&message.replace("\r\n", "\n"), "")
        .into_owned();
    let location = find_location(&plain).map(|(l, _)| l).unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    for raw in plain.lines() {
        if is_noise(raw) {
            continue;
        }
        let Some(text) = strip_header(raw) else {
            continue;
        };
        // A line that is only a source position says nothing the location does not.
        if PATH_LINE.is_match(&text) && PATH_LINE.replace_all(&text, "").trim().is_empty() {
            continue;
        }
        let scrubbed = scrub(&text);
        if scrubbed.is_empty() || scrubbed.chars().all(|c| !c.is_alphanumeric()) {
            continue;
        }
        lines.push(scrubbed);
        if lines.len() == SIGNATURE_LINES {
            break;
        }
    }
    let title: String = lines
        .first()
        .map(|l| l.chars().take(MAX_TITLE).collect())
        .unwrap_or_default();
    Normalised {
        location,
        title,
        body: lines.join("\n"),
    }
}

/// The failure signature: BLAKE3 over the job, the test and the normalised failure, each part
/// length prefixed so no two inputs share an encoding.
pub fn signature(job: &str, test: &str, n: &Normalised) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key("asc-ci failure signature v1");
    for part in [job, test, n.location.as_str(), n.body.as_str()] {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    *h.finalize().as_bytes()
}

/// Lower case hex of a signature.
pub fn signature_hex(sig: &[u8; 32]) -> String {
    sig.iter().map(|b| format!("{b:02x}")).collect()
}

/// Masks every occurrence of `secrets` (non-empty values) in `message` with `***`. The server
/// calls it with the values of the repository's secret rules before a message is stored.
pub fn mask(message: &str, secrets: &[&str]) -> String {
    let mut out = message.to_owned();
    for s in secrets.iter().filter(|s| s.len() >= 4) {
        out = out.replace(s, "***");
    }
    out
}

/// Cuts a message to the stored limit ([`MAX_MESSAGE`]) on a character boundary.
pub fn truncate_message(message: &str) -> String {
    cut(message, MAX_MESSAGE).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(m: &str) -> Normalised {
        normalise(m)
    }

    #[test]
    fn numbers_ids_and_times_are_removed() {
        let a = n("connection to 10.0.0.5:8080 timed out after 3.2s (request 4f9a1c2b77de)");
        let b = n("connection to 10.1.2.9:9091 timed out after 12.5s (request 00aa11bb22cc)");
        assert_eq!(a, b);
        assert_eq!(
            a.title,
            "connection to <addr> timed out after <dur> (request <hex>)"
        );
    }

    #[test]
    fn uuids_and_timestamps_are_removed() {
        let a = n("job 123e4567-e89b-12d3-a456-426614174000 failed at 2026-10-26T12:30:45.123Z");
        let b = n("job 00000000-0000-0000-0000-000000000001 failed at 2027-01-02 03:04:05");
        assert_eq!(a, b);
        assert_eq!(a.title, "job <uuid> failed at <ts>");
    }

    #[test]
    fn absolute_paths_in_the_message_are_removed() {
        let a = n("cannot open /tmp/build-1234/fixtures/a.json");
        let b = n("cannot open /var/folders/zz/T/build-99/fixtures/a.json");
        assert_eq!(a, b);
        assert_eq!(a.title, "cannot open <path>");
        let w = n(r"cannot open C:\Users\runner\AppData\Local\Temp\x\a.json");
        assert_eq!(w.title, "cannot open <path>");
    }

    #[test]
    fn a_rust_panic_keeps_its_file_but_not_its_line() {
        let a = n(
            "thread 'tests::adds' panicked at crates/asc-core/src/sum.rs:42:9:\nassertion `left == right` failed\n  left: 3\n right: 4\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace",
        );
        let b = n(
            "thread 'tests::adds' panicked at crates/asc-core/src/sum.rs:57:9:\nassertion `left == right` failed\n  left: 5\n right: 6\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace",
        );
        assert_eq!(a, b);
        assert_eq!(a.location, "crates/asc-core/src/sum.rs");
        assert_eq!(a.title, "assertion `left == right` failed");
        assert_eq!(
            a.body,
            "assertion `left == right` failed\nleft: <n>\nright: <n>"
        );
    }

    #[test]
    fn an_absolute_checkout_root_does_not_split_a_group() {
        let a = n(
            "thread 'x' panicked at /home/runner/work/asc/asc/crates/asc-core/src/sum.rs:42:9:\nboom",
        );
        let b =
            n("thread 'x' panicked at D:\\a\\asc\\asc\\crates\\asc-core\\src\\sum.rs:7:1:\nboom");
        assert_eq!(a.location, "crates/asc-core/src/sum.rs");
        assert_eq!(a, b);
    }

    #[test]
    fn the_old_rust_panic_format_is_read() {
        let a = n(
            "thread 'main' panicked at 'index out of bounds: the len is 3 but the index is 5', src/lib.rs:10:5",
        );
        assert_eq!(a.location, "src/lib.rs");
        assert_eq!(
            a.title,
            "index out of bounds: the len is <n> but the index is <n>"
        );
    }

    #[test]
    fn go_python_javascript_and_java_locations() {
        let go = n("--- FAIL: TestSum (0.00s)\n    sum_test.go:31: got 3, want 4");
        assert_eq!(go.location, "sum_test.go");
        assert_eq!(go.title, "--- FAIL: TestSum (<dur>)");
        let py = n(
            "Traceback (most recent call last):\n  File \"/work/repo/tests/test_sum.py\", line 8, in test_sum\n    assert add(1, 2) == 4\nAssertionError: 3 != 4",
        );
        assert_eq!(py.location, "tests/test_sum.py");
        assert_eq!(py.title, "assert add(<n>, <n>) == <n>");
        let js = n(
            "Error: expected 3 to equal 4\n    at Object.<anonymous> (/home/ci/app/src/sum.test.ts:14:23)\n    at node:internal/main:1:1",
        );
        assert_eq!(js.location, "src/sum.test.ts");
        let java = n(
            "java.lang.AssertionError: expected:<4> but was:<3>\n\tat org.junit.Assert.fail(Assert.java:89)\n\tat com.acme.SumTest.adds(SumTest.java:21)",
        );
        assert_eq!(java.location, "Assert.java");
        assert_eq!(
            java.title,
            "java.lang.AssertionError: expected:<<n>> but was:<<n>>"
        );
    }

    #[test]
    fn different_failures_stay_different() {
        let a = n("thread 't' panicked at src/a.rs:1:1:\nassertion failed: x > 0");
        let b = n("thread 't' panicked at src/a.rs:1:1:\nassertion failed: x < 0");
        let c = n("thread 't' panicked at src/b.rs:1:1:\nassertion failed: x > 0");
        assert_ne!(signature("j", "t", &a), signature("j", "t", &b));
        assert_ne!(signature("j", "t", &a), signature("j", "t", &c));
    }

    #[test]
    fn the_signature_depends_on_job_and_test_without_ambiguity() {
        let m = n("boom");
        assert_ne!(signature("a", "bc", &m), signature("ab", "c", &m));
        assert_ne!(signature("a", "b", &m), signature("a", "c", &m));
        assert_eq!(signature("a", "b", &m), signature("a", "b", &n("boom")));
        assert_eq!(signature_hex(&signature("a", "b", &m)).len(), 64);
    }

    #[test]
    fn ansi_colours_and_crlf_are_ignored() {
        let a = n("\u{1b}[31mFAILED\u{1b}[0m: value was 7\r\n");
        let b = n("FAILED: value was 9\n");
        assert_eq!(a, b);
    }

    #[test]
    fn an_empty_or_noise_only_message_normalises_to_nothing() {
        let e = n("");
        assert_eq!(
            (e.location.as_str(), e.title.as_str(), e.body.as_str()),
            ("", "", "")
        );
        let noise = n("   \n----\n==== \nnote: run with `RUST_BACKTRACE=1`");
        assert!(noise.body.is_empty());
    }

    #[test]
    fn a_huge_message_is_bounded_and_does_not_split_a_character() {
        let big = "é".repeat(MAX_MESSAGE);
        let got = n(&big);
        assert!(got.title.chars().count() <= MAX_TITLE);
        assert!(truncate_message(&big).len() <= MAX_MESSAGE);
        assert!(truncate_message(&big).is_char_boundary(truncate_message(&big).len()));
    }

    #[test]
    fn only_the_first_lines_count() {
        let a = n("failed\nsecond\nthird\nfourth line differs 1");
        let b = n("failed\nsecond\nthird\nfourth line differs 2");
        assert_eq!(signature("j", "t", &a), signature("j", "t", &b));
    }

    #[test]
    fn masking_removes_secret_values() {
        let masked = mask(
            "token ghp_abcdef123 leaked ghp_abcdef123",
            &["ghp_abcdef123", "x"],
        );
        assert_eq!(masked, "token *** leaked ***");
        assert_eq!(mask("keep x", &["x"]), "keep x");
    }

    #[test]
    fn normalise_path_forms() {
        assert_eq!(normalise_path("./src/a.rs"), "src/a.rs");
        assert_eq!(normalise_path("../x/src/a.rs"), "x/src/a.rs");
        assert_eq!(normalise_path("/src/app/src/lib.rs"), "src/lib.rs");
        assert_eq!(normalise_path("/opt/thing/x.rs"), "thing/x.rs");
        assert_eq!(normalise_path("C:/w/repo/tests/a.rs"), "tests/a.rs");
    }
}
