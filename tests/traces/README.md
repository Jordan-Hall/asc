# Tool trace corpus

Recorded git invocations of the tools that sit in front of asc (docs/SPEC.md §17.1, PLAN U25a):
Claude Code, husky, lint-staged, commitlint, turbo and ASC's repo scripts. Each trace is the
git traffic of one scripted user action, replayed against real git and against the asc shim.

```
tests/traces/
  <tool>/<case>.trace   the traces, and <tool>/README.md (versions, provenance)
  pins.toml             the pinned version of each tool
  pending.toml          known gaps between the corpus and the native table
  record/               the recorder (README.md there)
  main.rs model.rs replay.rs   the suite, built as the `traces` test of the `asc` package
```

## Running

```
cargo test -p asc --test traces                 # one test per trace, plus the corpus checks
cargo test -p asc --test traces -- lint_staged   # a tool
ASC_TRACES_KEEP=1 cargo test -p asc --test traces -- husky_hooks   # keep the fixtures and scenario.sh
ASC_TRACES_STRICT=1 cargo test -p asc --test traces                # the compat gate: no pending entry
```

For each trace the suite

1. checks the file (unknown keys, host paths, environment variables outside a short list and
   anything over a named limit are refused: `model.rs`);
2. replays it with real git and requires every recorded exit code (the recording is only valid
   if it replays);
3. replays it through the asc shim in an asc workspace on the compat runner (`tests/compat`) and
   requires the same stdout, stderr, exit codes and post-state as git. `${GITDIR}` in a trace is
   whatever `git rev-parse --absolute-git-dir` reports on the side being replayed, because an asc
   workspace keeps its git directory outside `<root>/.git`.

An invocation outside the native table (SPEC §11.5) fails with `fatal: asc: … is not supported`.
Every gap is listed in `pending.toml` with the unit that closes it; the suite fails on an
unlisted gap, warns about a stale entry, and `ASC_TRACES_STRICT=1` fails on any entry.

## Trace format

TOML. Header: `tool`, `version`, `provenance` (`recorded`, `authored` with `recapture`,
`extracted` with `source`), `git`, `driver`. Then `[[step]]` entries in order, each exactly one of

* `git = [argv…]` with `exit`, optional `env`, `stdin`, `routes = "real"` (the shim hands the
  invocation to real git by design, SPEC §11.4) and the `[[step.hook]]` tables of the git hooks
  that ran during it, each with `[[step.hook.call]]` entries;
* `sh = "…"`, a non-git action (a file edit, or what a tool task did to the files);
* `hook_event = "…"` with `stdin` JSON and `stdout`, a Claude Code hook event that the git
  runner skips (it pins the contract of the asc hooks, unit U54w).

`${ROOT}` is the work tree root and `${GITDIR}` the git directory. The replay writes each
hook as a script that repeats the recorded calls and deletes it after the step.

## Limits of the recording

* The working directory is not recorded: every call ran at the fixture root.
* stdin is recorded only where a trace author wrote it (`credential fill`).
* Environment is the `GIT_*` variables git reports for the process (trace2 `def_param`), minus the
  recorder's own baseline.
* A tool's own file writes are not git invocations. They enter a trace through `asc-act` (a task)
  or are fixed up by the converter (`mkdir` before `git diff --output`).
* lint-staged picks a random hash for its backup; a trace keeps the value of its own
  recording, so re-recording rewrites it.
* Commit and stash ids named in a trace (`stash store <id>`, a hook argument) are the ids the replay
  scenario produces under the fixed identity and dates. They change whenever a fixture file changes
  (a rename of text in a scripted file, say): run the trace with `ASC_TRACES_KEEP=1` and copy the
  new ids from the scenario output.
