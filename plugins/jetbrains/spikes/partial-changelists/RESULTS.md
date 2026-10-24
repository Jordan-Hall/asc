# Spike P53: partial-line commits (SPEC 24.5.4)

Question: can `AscCheckinEnvironment` offer `arePartialChangelistsSupported` with a `ReadFile`-backed `ContentRevision`
and a patch for `Workspace.Stage`?

## Verdict: NOT PROVEN. v1 ships whole-file commits plus `asc-ui` line staging.

Per the decision rule, `AscCheckinEnvironment` keeps whole-file commits and SPEC 24.3.1 row "Stage ... selected lines"
stays `-` for JetBrains in the Commit window until the platform half below passes.

## What the spike proves (pure Kotlin, `PartialPatchTest`)

- The asc half is feasible. `PartialPatch.build(path, base, current, selectedLineIds)` takes base content (what
  `Workspace.ReadFile` returns for the parent revision) and current content, and emits the `git apply --cached` text
  that `WsStageRequest.patch` (field 4) already carries. All 2^n subsets of a 4-change fixture reproduce the expected
  partial content when the patch is applied.
- Fail-closed inputs fall back to whole-file commit: path with `..`, leading `-`, absolute, backslash, quote, control
  characters; missing final newline; NUL (binary); files over 4M line pairs.

## What it does not prove (needs a live IDE, 2026.1 and 2026.2)

1. That the platform line-status tracker (`PartialLocalLineStatusTracker`) is created for a `ChangeListManager` file
   backed by `AscChangeProvider` and an `AscContentRevision` rather than a Git4Idea revision.
2. That the Commit window passes the tracker's selected ranges to `CheckinEnvironment.commit` (via
   `CommitContext`/`LineStatusTrackerManager`) instead of whole `Change` objects.
3. That after `Workspace.Stage` with the patch, the unselected lines remain in the work tree and show as still-modified.

Next step: add item 1-3 as an `IdeTestCase` in `plugins/jetbrains/src/test`, then wire `PartialPatch` into
`AscCheckinEnvironment` and return true from `arePartialChangelistsSupported`.

## Status of verification

`gradle` and `kotlinc` are not installed on the authoring machine, so `PartialPatchTest` was written but not executed.
The root `settings.gradle.kts` (owned by P36) does not include this project; run it standalone
(`gradle test` in this directory) or add `include(":spikes:partial-changelists")` with
`project(":spikes:partial-changelists").projectDir = file("spikes/partial-changelists")`.
