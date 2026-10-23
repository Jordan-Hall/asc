# Recorder

Records the git invocations of a pinned tool by running it against real git under
`GIT_TRACE2_EVENT`. No wrapper is needed, so a tool can spawn git any way it likes
(cross-spawn, execa, Rust `Command`). SPEC §17.1 allows a recording `git` wrapper or
`GIT_TRACE=1`; trace2 gives the argv, the parent/child structure (which calls a hook made) and
the `GIT_*` environment, which `GIT_TRACE` does not.

## Use

```
cd tests/traces/record/tools && npm ci          # the pinned tools (package-lock.json)
ASC_TRACE_TOOLS=$PWD tests/traces/record/record-all.sh                # every scenario
ASC_TRACE_TOOLS=$PWD tests/traces/record/record-all.sh husky/hooks    # one
```

`record.sh <scenario.sh> <out.trace>` creates a temporary `HOME` with the compat suite's
`.gitconfig`, the same fixed environment as the compat runner (so object ids equal those of a
replay), runs the scenario in a fresh work tree and converts the trace2 log with `to-trace.mjs`.
`ASC_TRACE_KEEP=1` keeps the fixture. It needs Git Bash (or any POSIX `sh`), node and, for the
turbo scenarios, bun.

## Scenarios

`scenarios/<tool>/<case>.sh` is sourced by `record.sh`. Its first lines are metadata
(`#@ tool:`, `#@ version:`, `#@ provenance:`, `#@ driver:`). Helpers (`prelude.sh`):

| Helper | Meaning |
|---|---|
| `g <git args>` | a git command typed by the user; a non-zero exit is recorded, not fatal |
| `act '<snippet>' [args]` | a non-git action; the snippet is stored in the trace and replayed with `sh` |
| `tool <name> <cmd…>` | run a tool; the git commands it runs directly are the tool's |

A tool task that edits files goes through `asc-act --file <script>` (`bin/`, on `PATH`), which
runs the script and stores it as an `act` step at the right place. Fixture files (configs, task
scripts, hooks) are created with `act`, so a replay rebuilds the same tree and the recorded
object ids hold; they must not name host paths.

## Conversion

`to-trace.mjs` keeps the git processes that the scenario or the tool started, drops git's own
helpers (`maintenance`, fsmonitor) and attaches the calls made while git ran a hook to the git
step that ran it. It rewrites the fixture root to `${ROOT}` and the git directory to
`${GITDIR}`, marks the invocations the shim hands to real git (`routes = "real"`, mirroring
`independent` in `crates/asc/src/shim.rs`), and adds the `mkdir` that a tool does before
`git diff --output`.

## Not recorded

Claude Code. Driving it needs a model session; its traces are authored from the git argument
templates in the pinned build and marked `recapture = "S7"` (docs/SPEC.md §19). To record them,
run the pinned build under the same environment (`GIT_TRACE2_EVENT`, `GIT_TRACE2_ENV_VARS`) in
a fixture and convert with `to-trace.mjs`.
