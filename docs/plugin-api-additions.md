# asc API additions for the IDE plugins (P57)

SPEC §24.8.2 A1, A2 and A4. All three are additive: a client that ignores the new fields keeps working.

## A1. `WsStack.behind`

`behind` is the number of trunk commits that are not in the stack base: the commits reachable from the trunk tip and not from `base`. The count stops at 10,000 (the value is then 10,000). It is 0 when the stack sits on the trunk tip. No field was added; this section is the documented meaning. The IDE "trunk moved N commits - Restack" item shows it and hides itself at 0.

## A2. `CommitGraphResponse.total` and `ref_markers`

```proto
message RefMarker { string name = 1; uint64 row = 2; }
message CommitGraphResponse { ...; uint64 total = 3; repeated RefMarker ref_markers = 4; }
```

Only the first message of the first page (no `page_token`) carries them, in the repository and the workspace variant alike. Later pages and later messages of the first page never do.

- `ref_markers[]` names each branch or tag that labels a row of the first page. `row` is the zero-based index of that row among the rows of the first page, work-in-progress rows included. At most 5,000 markers.
- `total` is the row count of the whole result. It is set when the first page is also the last page. When more pages follow, the count is not known without walking them, so `total` is 0, which means "unknown", and the client sizes the minimap from the pages it has loaded.

A server may omit both. Code: `crates/asc-serve/src/commit_graph_markers.rs` (repository) and `crates/asc/src/stack/cli/ws_api.rs` (workspace).

## A4. Target commands

A `[[project]]` of `.asc/projects.toml` may define the command for each target:

```toml
[[project]]
select = "node:@asc/web"
argv = { test = ["bun", "run", "test"], lint = ["eslint", "."] }
```

`AffectedProject` gains `map<string, TargetCmd> targets = 5` with `message TargetCmd { repeated string argv = 1; }`, and `asc affected --json` prints `"targets": { "<target>": { "argv": [...] } }` for a project that has any. asc stores and serves the table and never runs it; an IDE task runs it with the project root as the working directory. When several entries apply to one project, a later entry replaces the command of the same target.

Limits, checked when the file is parsed (a violation is an error that names the line): at most 64 targets per project, a target name of 1 to 128 bytes without whitespace or control characters, a non-empty command with a non-empty program name, at most 256 words, 4,096 bytes per word and 32 KiB per command, and no NUL byte. Code: `crates/asc-graph/src/targets.rs`.
