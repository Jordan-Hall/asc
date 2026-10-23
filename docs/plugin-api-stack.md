# Plugin API: stacks, worktrees, conflicts, commit graph, buffer blame

Additions to the Workspace plugin RPCs (SPEC 20.5 and 20.6, unit C31). The rules of
[`plugin-api.md`](plugin-api.md) hold for each of them: only the current user connects, the workspace
must be mounted (`FAILED_PRECONDITION` otherwise), the root is checked again before each query, text is
at most 1,024 bytes, an answer is at most 16 MiB, a stream line at most 16 MiB, a stream at most 256 MiB,
and a query runs at most 300 s. Ids are the ids of the asc JSON profile.

| RPC | Request | Answer | Equivalent |
| --- | --- | --- | --- |
| `Stacks` | `ws_id`, `all`, `remote`, `revset` | `WsStacksResponse` (`version`, `trunks`, `stacks`) | `asc sl --json [-a] [--remote] [--revset=<r>]` |
| `Worktrees` | none | `WsWorktreesResponse` | `asc ws-api worktrees` |
| `Conflicts` | `ws_id` | `WsConflictsResponse` | `asc ws-api conflicts` |
| `CommitGraph` | `ws_id`, `query` (`CommitGraphRequest`), `wip_rows` | stream of `CommitGraphResponse` | `asc ws-api graph --include=<r> --exclude=<r> --preset=<n> [--lanes] [--stats] [--limit=<n>] [--page-token=<t>] [--wip]` |
| `Blame` (additions) | `buffer`, `ignore_whitespace` | as `Blame` | `asc blame --json [-w]`, plus the mapping below |

`asc ws-api` is plumbing for the daemon and the tests; its output is the JSON profile of the response
message, one line per message.

## `Stacks`

The answer is `asc sl --json` as a typed message (SPEC 20.7). `StackEntry.author.tz_min` is the zone in
minutes east of UTC; `review` is set only with `remote`; `WsStack.behind` counts trunk commits that are
not in the stack base. Hex ids in the JSON are bytes here (git commit ids 20 bytes, changeset ids 32 bytes); a change id is its
16 bytes.

## `Worktrees`

The request has no workspace: the daemon lists the worktrees of every repository that has a mounted
workspace, one repository after another (the daemon touches only workspaces it hosts). A repository with
no mounted workspace is not listed. At most 10,000 rows per repository.

- `head_ref` is empty for a detached HEAD; `head` is the commit's git id (empty when unborn).
- `dirty` is the number of distinct paths in the workspace journal. A virtual workspace journals every
  write; a plain workspace journals only while someone watches it, so its `dirty` can lag.
- `staged` counts staged paths (stage 0 rows). `ahead` and `behind` compare the branch with its
  upstream (`branch.<name>.remote` and `.merge`), each capped at 999; both are 0 without an upstream.
- `ephemeral_until` is Unix seconds, 0 when the worktree is not ephemeral. `locked` is the lock reason.
- `in_progress` is `merge`, `rebase`, `cherry-pick`, `revert`, `bisect` or empty.

## `Conflicts`

One row per path with staging stages 1 to 3 in the workspace's staging area, in path order, at most
10,000. `base`, `ours` and `theirs` are git blob ids (empty when that side has no entry). `kind` is
`ADD_ADD` (no base, both sides), `MODIFY_DELETE` (one side deleted) or `CONTENT`. `marked` is the file in
the work tree (with its conflict markers); it is empty when the file is missing, is a link or sits below
one, is not a regular file, or is larger than 4 MiB.

## `CommitGraph`

`query` is the `CommitGraphRequest` of the server RPC; `repo` is ignored. Pages come from the
engine's generation-ordered walk; a page is at most 2,000 rows (default 200) in messages of 100 rows.
The lane state rides in `next_page_token`; the token is hex, at most 8,192 digits, and a token whose
state has expired is `FAILED_PRECONDITION` (restart from the first page). `include` and `exclude` take
at most 64 revisions each. With no `include`, the preset picks the start points: `ALL` every branch and
tag, `CURRENT` HEAD, `SMART` HEAD and the branches, `STACK` the branches other than the trunk with the
trunk hidden (the trunk is the first `asc.stack.trunk`, else `master`). With `stats` a call derives at
most 16 uncached `diffstat.v1`; rows past that carry no `stats` and a repeat fills them. An entry's
`id` is the changeset id, `git` the git commit id, `parents` are changeset ids and `change` is the
change id.

With `wip_rows`, the first page starts with one row per worktree that has uncommitted work (`dirty` or
`staged` above 0): `wip = true`, `ws_id`, an empty `entry.id`, and `entry.parents` holding the
worktree's HEAD changeset. WIP rows do not count against `limit`.

## `Blame` with a buffer

An empty `buffer` is the working copy as before. A non-empty `buffer` (at most 8 MiB, and a request is
at most 4 MiB on the wire) is the editor's unsaved text. The daemon blames the file at `rev` (default
`HEAD`), reads the file's blob at `rev`, maps each buffer line to a blob line by one line diff (Myers, at most
500,000 lines), and answers hunks over the buffer's lines. A line that matches a blob line keeps
that line's origin; any other line is uncommitted (an empty `origin`, `orig_path` the path, `orig_start`
the line). `start_line` and `end_line` select buffer lines. A path the revision lacks makes every line
uncommitted. `ignore_whitespace` passes `-w` to the blame and ignores whitespace in the line diff.
Buffers that differ from the blob by a rewritten region map nothing in that region, as `git blame
--contents` does.
