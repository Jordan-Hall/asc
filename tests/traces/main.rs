//! U25a: the tool trace corpus (docs/SPEC.md §17.1, docs/PLAN.md U25a).
//!
//! `tests/traces/<tool>/<case>.trace` records the git invocations that Claude Code, husky,
//! lint-staged, commitlint, turbo and ASC's repo scripts make, at the pinned versions in
//! `pins.toml`, together with the scripted user action that drove them. This suite
//!
//! 1. parses and validates every trace and checks the corpus covers each tool and each form that
//!    SPEC §11.5 names;
//! 2. replays every trace against real git and checks every exit code against the recording;
//! 3. replays it against the asc shim in an asc workspace, on the compat runner, and requires the
//!    same stdout, stderr, exit codes and post-state as git. An invocation outside the native
//!    table fails with `fatal: asc: … is not supported`; each such gap must be listed in
//!    `pending.toml` with the unit that closes it, and `ASC_TRACES_STRICT=1` (the compat gate)
//!    refuses any pending entry.
//!
//! Recording is `record/record.sh`; see `README.md`.

#[path = "../compat/src/lib.rs"]
mod harness;
mod model;
mod replay;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use harness::{Side, Tools, compare};
use model::{MAX_TRACE_BYTES, Provenance, Trace};
use regex::Regex;
use serde::Deserialize;

/// The tools in scope (SPEC §17.1). Each has a README and at least one trace.
const TOOLS: &[&str] = &["claude-code", "husky", "lint-staged", "commitlint", "turbo", "asc-scripts"];

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/traces")
}

fn tools() -> &'static Tools {
    match Tools::get(env!("CARGO_BIN_EXE_asc")) {
        Ok(t) => t,
        Err(e) => panic!("trace tools: {e}"),
    }
}

/// Every `<tool>/<case>` name in the corpus, sorted.
fn trace_names() -> Vec<String> {
    let mut names = Vec::new();
    for dir in fs::read_dir(corpus()).expect("corpus dir").flatten() {
        let tool = dir.file_name().to_string_lossy().into_owned();
        if !dir.path().is_dir() || tool == "record" {
            continue;
        }
        for file in fs::read_dir(dir.path()).expect("tool dir").flatten() {
            let file = file.file_name().to_string_lossy().into_owned();
            if let Some(case) = file.strip_suffix(".trace") {
                names.push(format!("{tool}/{case}"));
            }
        }
    }
    names.sort();
    names
}

fn load(name: &str) -> Trace {
    let path = corpus().join(format!("{name}.trace"));
    let meta = fs::metadata(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert!(meta.len() <= MAX_TRACE_BYTES, "{name}: {} bytes exceeds the {MAX_TRACE_BYTES} byte limit", meta.len());
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    Trace::parse(name, &text.replace("\r\n", "\n")).unwrap_or_else(|e| panic!("{e}"))
}

// ---- pins and pending ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Pins {
    /// The git version prefix the traces were recorded against.
    git: String,
    tool: BTreeMap<String, Pin>,
}

#[derive(Debug, Deserialize)]
struct Pin {
    version: String,
    /// Where the pinned build comes from (a package, a build, a commit).
    source: String,
}

fn pins() -> Pins {
    let text = fs::read_to_string(corpus().join("pins.toml")).expect("pins.toml");
    toml::from_str(&text).unwrap_or_else(|e| panic!("pins.toml: {e}"))
}

#[derive(Debug, Deserialize)]
struct PendingFile {
    #[serde(default)]
    pending: Vec<Pending>,
}

/// A known gap between a trace and the native table.
#[derive(Debug, Deserialize)]
struct Pending {
    trace: String,
    /// The PLAN unit that closes the gap.
    unit: String,
    reason: String,
    /// The `asc: … is not supported` messages the asc side prints, without the `asc: ` prefix.
    #[serde(default)]
    unsupported: Vec<String>,
    /// The trace runs, but asc's output or post-state still differs from git's.
    #[serde(default)]
    differs: bool,
}

fn pending() -> Vec<Pending> {
    let text = fs::read_to_string(corpus().join("pending.toml")).expect("pending.toml");
    let file: PendingFile = toml::from_str(&text).unwrap_or_else(|e| panic!("pending.toml: {e}"));
    for p in &file.pending {
        assert!(!p.reason.trim().is_empty() && p.unit.starts_with('U'), "pending.toml: {p:?} needs a unit and a reason");
        assert!(p.differs || !p.unsupported.is_empty(), "pending.toml: {p:?} lists nothing");
    }
    file.pending
}

fn strict() -> bool {
    std::env::var_os("ASC_TRACES_STRICT").is_some_and(|v| !v.is_empty())
}

// ---- corpus checks ------------------------------------------------------------------------

#[test]
fn every_trace_parses() {
    let names = trace_names();
    assert!(names.len() >= 12, "only {} traces", names.len());
    for name in &names {
        load(name);
    }
}

#[test]
fn every_tool_has_a_readme_and_a_trace() {
    let names = trace_names();
    for tool in TOOLS {
        let readme = corpus().join(tool).join("README.md");
        let text = fs::read_to_string(&readme).unwrap_or_else(|e| panic!("{}: {e}", readme.display()));
        assert!(text.contains("## Versions") && text.contains("## Provenance"), "{tool}/README.md lacks Versions or Provenance");
        assert!(names.iter().any(|n| n.starts_with(&format!("{tool}/"))), "no trace for {tool}");
    }
    let known: BTreeSet<&str> = TOOLS.iter().copied().collect();
    for name in &names {
        let tool = name.split('/').next().unwrap_or_default();
        assert!(known.contains(tool), "{name}: {tool} is not a tool in scope");
    }
}

#[test]
fn traces_match_the_pinned_versions() {
    let pins = pins();
    for tool in TOOLS {
        let pin = pins.tool.get(*tool).unwrap_or_else(|| panic!("pins.toml has no entry for {tool}"));
        assert!(!pin.version.is_empty() && !pin.source.is_empty(), "pins.toml: {tool} needs a version and a source");
    }
    for name in trace_names() {
        let trace = load(&name);
        let pin = pins.tool.get(&trace.tool).unwrap_or_else(|| panic!("{name}: no pin for {}", trace.tool));
        assert_eq!(trace.version, pin.version, "{name}: version differs from pins.toml");
        assert!(trace.git.starts_with(&pins.git), "{name}: recorded against git {}, pinned {}", trace.git, pins.git);
        assert!(name.starts_with(&format!("{}/", trace.tool)), "{name}: sits under the wrong tool directory");
    }
}

#[test]
fn recorded_traces_carry_no_machine_paths() {
    let host = Regex::new(r#"(?i)(^|[^A-Za-z0-9])[a-z]:[\\/]|/users/|appdata|/tmp/|/home/|\btmp\.[A-Za-z0-9]{6}"#).unwrap();
    for name in trace_names() {
        let text = fs::read_to_string(corpus().join(format!("{name}.trace"))).unwrap();
        // The header comment names the recorder script; the body is what a replay reads.
        for (n, line) in text.lines().enumerate().filter(|(_, l)| !l.starts_with('#')) {
            assert!(!host.is_match(line), "{name}:{}: a host path in a trace: {line}", n + 1);
        }
    }
}

/// Every git call of every trace as (tool, argv, env, in a hook).
fn all_calls() -> Vec<(String, Vec<String>, BTreeMap<String, String>)> {
    let mut calls = Vec::new();
    for name in trace_names() {
        let trace = load(&name);
        for step in &trace.steps {
            if let Some(argv) = &step.git {
                calls.push((trace.tool.clone(), argv.clone(), step.env.clone()));
            }
            for call in step.hooks.iter().flat_map(|h| &h.calls) {
                if let Some(argv) = &call.git {
                    calls.push((trace.tool.clone(), argv.clone(), call.env.clone()));
                }
            }
        }
    }
    calls
}

/// Every invocation form SPEC §11.5 says the recorded traces must define.
#[test]
fn corpus_defines_the_forms_the_native_table_needs() {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut env: BTreeSet<String> = BTreeSet::new();
    for (tool, argv, e) in all_calls() {
        // The command is the first word that is not a global option or its value.
        let mut words = argv.iter().peekable();
        while let Some(w) = words.peek() {
            if *w == "-C" || *w == "-c" {
                words.next();
                words.next();
            } else if w.starts_with('-') {
                words.next();
            } else {
                break;
            }
        }
        let rest: Vec<&String> = words.collect();
        if let Some(cmd) = rest.first() {
            seen.insert(format!("{tool} {cmd}"));
            if *cmd == "stash"
                && let Some(sub) = rest.get(1)
            {
                seen.insert(format!("{tool} stash {sub}"));
            }
        }
        for a in &argv {
            seen.insert(format!("{tool} {}", a.split('=').next().unwrap_or(a)));
        }
        for k in e.keys() {
            env.insert(format!("{tool} {k}"));
        }
    }
    let mut hooks: BTreeSet<String> = BTreeSet::new();
    for name in trace_names() {
        let trace = load(&name);
        for hook in trace.steps.iter().flat_map(|s| &s.hooks) {
            hooks.insert(format!("{} {}", trace.tool, hook.name));
        }
    }
    let need = |what: &str, set: &BTreeSet<String>, items: &[&str]| {
        let missing: Vec<&&str> = items.iter().filter(|i| !set.contains(**i)).collect();
        assert!(missing.is_empty(), "the corpus lacks {what}: {missing:?}");
    };
    need(
        "lint-staged forms (SPEC §11.5)",
        &seen,
        &[
            "lint-staged stash create",
            "lint-staged stash store",
            "lint-staged diff",
            "lint-staged --binary",
            "lint-staged --output",
            "lint-staged --src-prefix",
            "lint-staged --submodule",
            "lint-staged apply",
            "lint-staged --recount",
            "lint-staged --unidiff-zero",
            "lint-staged --whitespace",
            "lint-staged --3way",
            "lint-staged reset",
        ],
    );
    need("turbo forms (spike S4)", &seen, &["turbo rev-parse", "turbo ls-files", "turbo --others", "turbo diff-tree", "turbo --merge-base", "turbo show", "turbo diff"]);
    need("commitlint forms", &seen, &["commitlint log", "commitlint merge-base", "commitlint config"]);
    need(
        "Claude Code forms (SPEC §11.5)",
        &seen,
        &["claude-code status", "claude-code diff", "claude-code log", "claude-code worktree", "claude-code branch", "claude-code push"],
    );
    need("husky hooks (SPEC §17.1)", &hooks, &["husky pre-commit", "husky commit-msg", "husky post-checkout", "husky pre-push"]);
    need("the GIT_INDEX_FILE flow of hook calls (SPEC §11.5)", &env, &["husky GIT_INDEX_FILE"]);
    need("a stdin invocation", &seen, &["asc-scripts credential"]);
}

#[test]
fn claude_code_worktrees_are_covered_with_and_without_hooks() {
    let names = trace_names();
    for case in ["worktree-default", "worktree-git-hooks", "worktree-hook-commands"] {
        assert!(names.iter().any(|n| n == &format!("claude-code/{case}")), "claude-code/{case} is missing");
    }
    let hooked = load("claude-code/worktree-hook-commands");
    let events: Vec<&str> = hooked.steps.iter().filter_map(|s| s.hook_event.as_deref()).collect();
    assert_eq!(events, ["WorktreeCreate", "WorktreeRemove"]);
    assert!(hooked.recapture.as_deref() == Some("S7"), "the hook contract stays authored until spike S7");
}

#[test]
fn provenance_is_honest() {
    for name in trace_names() {
        let trace = load(&name);
        let recorded = corpus().join("record").join("scenarios").join(format!("{name}.sh"));
        match trace.provenance {
            Provenance::Recorded => assert!(recorded.is_file(), "{name}: recorded, but {} does not exist", recorded.display()),
            Provenance::Authored | Provenance::Extracted => {
                assert!(!recorded.exists(), "{name}: has a recorder scenario, so it is recorded");
            }
        }
    }
}

// ---- model validation ---------------------------------------------------------------------

mod validation {
    use super::model::Trace;

    const HEAD: &str = "tool = \"t\"\nversion = \"1\"\nprovenance = \"recorded\"\ngit = \"2.55.0\"\ndriver = \"d\"\n";

    fn parse(body: &str) -> Result<Trace, String> {
        Trace::parse("t", &format!("{HEAD}{body}"))
    }

    #[test]
    fn a_minimal_trace_parses() {
        parse("[[step]]\ngit = [\"init\", \"-q\"]\nfrom = \"driver\"\nexit = 0\n").unwrap();
    }

    #[test]
    fn bad_traces_are_refused() {
        let cases: &[(&str, &str)] = &[
            ("unknown key", "bogus = 1\n[[step]]\ngit = [\"init\"]\nfrom = \"d\"\nexit = 0\n"),
            ("no steps", ""),
            ("two kinds", "[[step]]\ngit = [\"init\"]\nsh = \"true\"\nfrom = \"d\"\nexit = 0\n"),
            ("empty argv", "[[step]]\ngit = []\nfrom = \"d\"\nexit = 0\n"),
            ("missing exit", "[[step]]\ngit = [\"init\"]\nfrom = \"d\"\n"),
            ("host path", "[[step]]\ngit = [\"add\", \"/etc/passwd\"]\nfrom = \"d\"\nexit = 0\n"),
            ("drive path", "[[step]]\ngit = [\"add\", \"C:/x\"]\nfrom = \"d\"\nexit = 0\n"),
            ("climbing path", "[[step]]\ngit = [\"add\", \"../../x\"]\nfrom = \"d\"\nexit = 0\n"),
            ("PATH override", "[[step]]\ngit = [\"init\"]\nfrom = \"d\"\nexit = 0\nenv = { PATH = \"/x\" }\n"),
            ("exec path override", "[[step]]\ngit = [\"init\"]\nfrom = \"d\"\nexit = 0\nenv = { GIT_EXEC_PATH = \"/x\" }\n"),
            ("unknown hook", "[[step]]\ngit = [\"commit\"]\nfrom = \"d\"\nexit = 0\n[[step.hook]]\nname = \"pre-nothing\"\npath = \".git/hooks/x\"\nexit = 0\n[[step.hook.call]]\nsh = \"true\"\nfrom = \"t\"\n"),
            ("hook path out of tree", "[[step]]\ngit = [\"commit\"]\nfrom = \"d\"\nexit = 0\n[[step.hook]]\nname = \"pre-commit\"\npath = \"../hooks/pre-commit\"\nexit = 0\n[[step.hook.call]]\nsh = \"true\"\nfrom = \"t\"\n"),
            ("hook event without JSON", "[[step]]\nhook_event = \"WorktreeCreate\"\nfrom = \"c\"\n"),
            ("NUL", "[[step]]\ngit = [\"commit\", \"-m\", \"a\\u0000b\"]\nfrom = \"d\"\nexit = 0\n"),
        ];
        for (what, body) in cases {
            assert!(parse(body).is_err(), "{what} must be refused");
        }
    }

    #[test]
    fn a_sibling_bare_repository_is_the_one_allowed_escape() {
        parse("[[step]]\ngit = [\"remote\", \"add\", \"origin\", \"../origin.git\"]\nfrom = \"d\"\nexit = 0\n").unwrap();
        assert!(parse("[[step]]\ngit = [\"remote\", \"add\", \"origin\", \"../origin\"]\nfrom = \"d\"\nexit = 0\n").is_err());
    }

    #[test]
    fn authored_and_extracted_traces_name_their_origin() {
        let body = "[[step]]\ngit = [\"init\"]\nfrom = \"d\"\nexit = 0\n";
        let authored = format!("{}{body}", HEAD.replace("recorded", "authored"));
        assert!(Trace::parse("t", &authored).is_err());
        let extracted = format!("{}{body}", HEAD.replace("recorded", "extracted"));
        assert!(Trace::parse("t", &extracted).is_err());
    }
}

// ---- replay -------------------------------------------------------------------------------

/// The `asc: … is not supported` messages in asc's stderr, each with the `git` invocation that drew
/// it, in order of appearance (one entry per message).
fn unsupported_messages(stderr: &str) -> Vec<(String, String)> {
    let re = Regex::new(r"asc: ((?:option )?'[^'\n]*' is not supported(?: yet| in an asc workspace)?)").unwrap();
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut invocation = String::new();
    for line in stderr.lines() {
        if let Some(cmd) = line.strip_prefix("$ git ") {
            invocation = cmd.to_owned();
        } else if let Some(cap) = re.captures(line) {
            let msg = cap[1].to_owned();
            if !seen.iter().any(|(m, _)| *m == msg) {
                seen.push((msg, invocation.clone()));
            }
        }
    }
    seen
}

fn exits(base: &Path, side: Side) -> Vec<(String, i32)> {
    let dir = match side {
        Side::Git => "git",
        Side::Asc => "asc",
    };
    let text = fs::read_to_string(base.join(dir).join("exits.log")).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let (label, rc) = l.split_once(' ')?;
            Some((label.to_owned(), rc.parse().ok()?))
        })
        .collect()
}

fn text(bytes: Option<&Vec<u8>>) -> String {
    bytes.map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default()
}

fn check_trace(name: &str) {
    let trace = load(name);
    let script = replay::script(&trace);
    let tools = tools();
    let tmp = tempfile::tempdir().expect("tempdir");
    // ASC_TRACES_KEEP=1 keeps the fixtures (and the generated scenario.sh) for inspection.
    let base = if std::env::var_os("ASC_TRACES_KEEP").is_some() {
        let kept = tmp.keep();
        eprintln!("traces: {name}: fixtures kept in {}", kept.display());
        kept
    } else {
        tmp.path().to_owned()
    };
    let base = base.as_path();

    // Real git first: the recording must replay with the exit codes it was recorded with.
    let git = tools.run_side(Side::Git, &script, base, false).unwrap_or_else(|e| panic!("{name}: git side: {e}"));
    let want = replay::expected_exits(&trace);
    let got = exits(base, Side::Git);
    assert_eq!(got, want, "{name}: real git replays with other exit codes than the recording\n{}", text(git.sections.get("stderr")));

    let asc = tools.run_side(Side::Asc, &script, base, false).unwrap_or_else(|e| panic!("{name}: asc side: {e}"));
    let routed = replay::real_routed(&trace);
    assert_eq!(asc.fall_through, routed, "{name}: the asc side fell through to real git for other invocations than the ones the trace routes there");

    let found = unsupported_messages(&text(asc.sections.get("stderr")));
    let gaps: Vec<String> = found.iter().map(|(msg, _)| msg.clone()).collect();
    let all = pending();
    let entry = all.iter().find(|p| p.trace == name);
    if strict() {
        assert!(entry.is_none(), "{name}: strict mode refuses the pending entry {entry:?}");
    }
    let listed: Vec<&String> = entry.map(|p| p.unsupported.iter().collect()).unwrap_or_default();
    let new_gaps: Vec<&String> = gaps.iter().filter(|g| !listed.contains(g)).collect();
    if !new_gaps.is_empty() {
        let drawn: String = found.iter().map(|(msg, cmd)| format!("  {msg}   <- git {cmd}\n")).collect();
        panic!(
            "{name}: invocations outside the native table (SPEC §11.5). Close them in the owning unit or list them in pending.toml:\n{drawn}\
             [[pending]]\ntrace = \"{name}\"\nunit = \"U??\"\nreason = \"…\"\nunsupported = {gaps:?}\n"
        );
    }
    for stale in listed.iter().filter(|l| !gaps.contains(l)) {
        eprintln!("traces: {name}: pending.toml lists {stale:?}, which asc now serves; remove it");
    }
    if !gaps.is_empty() {
        return; // The asc side stopped following the recording at the first gap; nothing to compare.
    }
    let allowed = tools.allowed().unwrap_or_else(|e| panic!("{e}"));
    match compare(name, &git, &asc, &allowed) {
        Ok(()) => {
            if entry.is_some_and(|p| p.differs) {
                eprintln!("traces: {name}: asc now matches git; remove the pending entry");
            }
        }
        Err(report) => assert!(
            entry.is_some_and(|p| p.differs),
            "{report}\nlist the trace in pending.toml with `differs = true` and the unit that fixes it, or fix asc"
        ),
    }
}

/// One test per trace, so `cargo test -p asc --test traces -- <tool>` selects it. Add a line with
/// each new trace; `every_trace_has_a_test` fails otherwise.
macro_rules! traces {
    ($($test:ident = $file:literal),* $(,)?) => {
        $( #[test] fn $test() { check_trace($file); } )*
        const REGISTERED: &[&str] = &[$($file),*];
    };
}

traces! {
    claude_code_session_context = "claude-code/session-context",
    claude_code_edit_commit_push = "claude-code/edit-commit-push",
    claude_code_worktree_default = "claude-code/worktree-default",
    claude_code_worktree_git_hooks = "claude-code/worktree-git-hooks",
    claude_code_worktree_hook_commands = "claude-code/worktree-hook-commands",
    commitlint_edit_message = "commitlint/edit-message",
    commitlint_history_range = "commitlint/history-range",
    asc_check_schema_instances = "asc-scripts/check-schema-instances",
    asc_pr_pack_forge = "asc-scripts/pr-pack-forge",
    husky_hooks = "husky/hooks",
    lint_staged_partially_staged = "lint-staged/partially-staged",
    lint_staged_restore_conflict = "lint-staged/restore-conflict",
    lint_staged_task_failure = "lint-staged/task-failure",
    lint_staged_add_delete_rename = "lint-staged/add-delete-rename",
    turbo_dry_run = "turbo/dry-run",
    turbo_affected = "turbo/affected",
}

#[test]
fn every_trace_has_a_test() {
    let names = trace_names();
    let missing: Vec<_> = names.iter().filter(|n| !REGISTERED.contains(&n.as_str())).collect();
    assert!(missing.is_empty(), "traces without a test in tests/traces/main.rs: {missing:?}");
    let stale: Vec<_> = REGISTERED.iter().filter(|n| !names.iter().any(|m| m == *n)).collect();
    assert!(stale.is_empty(), "tests without a trace file: {stale:?}");
}

#[test]
fn pending_entries_name_real_traces() {
    let names = trace_names();
    for p in pending() {
        assert!(names.contains(&p.trace), "pending.toml: {} is not a trace", p.trace);
    }
}

#[test]
fn unsupported_messages_name_the_invocation_that_drew_them() {
    let stderr = "$ git status\n[exit 0]\n$ git stash create\nfatal: asc: 'stash' is not supported yet\n[exit 129]\n\
                  $ git diff --ita-visible-in-index\nfatal: asc: option '--ita-visible-in-index' is not supported\n\
                  $ git stash store\nfatal: asc: 'stash' is not supported yet\nfatal: asc: 'worktree' is not supported in an asc workspace\n";
    assert_eq!(
        unsupported_messages(stderr),
        [
            ("'stash' is not supported yet".to_owned(), "stash create".to_owned()),
            ("option '--ita-visible-in-index' is not supported".to_owned(), "diff --ita-visible-in-index".to_owned()),
            ("'worktree' is not supported in an asc workspace".to_owned(), "stash store".to_owned()),
        ]
    );
    assert!(unsupported_messages("fatal: not a git repository\n").is_empty());
}
