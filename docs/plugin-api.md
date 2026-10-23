# Plugin API (Workspace plugin RPCs)

The stable, documented API that ASC's IDE plugins use (SPEC §7 `workspace.proto`, §11.2, §24). It is
served by the per-user daemon on its local endpoint (named pipe or Unix socket, current user only) and
reached by plugins through the `asc api --stdio` bridge. Every RPC has an `asc … --json` command that prints
the same answer, so a plugin without a daemon, and every test, can use the CLI instead.

## Rules for every RPC

- **Authorisation.** Only the current user can connect. An RPC acts only on a workspace the daemon has
  mounted (`Mount` checks that the workspace is registered in `state.db` and that its root is a directory
  of the user reached through no link). A request for another workspace is `FAILED_PRECONDITION`. The
  root is checked again before each query runs.
- **Bounds.** Text (revision, ref, author) is at most 1,024 bytes, without NUL or line breaks, and a
  revision never starts with `-`. At most 10,000 paths per request and 16 KiB of path text for `Status`,
  `Diff`, `Log` and `Blame`; paths are valid repository paths in UTF-8, literal (no wildcards), and come
  after `--`. A one-message answer is at most 16 MiB, a stream line at most 16 MiB, a stream at most
  256 MiB, and one query runs at most 300 s. At most 128 `Watch` streams are open in the daemon.
- **Errors.** `INVALID_ARGUMENT` for a malformed request, `FAILED_PRECONDITION` with the CLI's message when
  the command fails (a bad revision, a patch that does not apply), `RESOURCE_EXHAUSTED` and
  `DEADLINE_EXCEEDED` for the bounds above.
- **Ids** are 20-byte git SHA-1s (§11.1). In the JSON form (asc JSON profile, §22.4.2) they are the
  encoding the profile defines for `bytes` fields.

## RPCs and their CLI equivalents

| RPC | Request | Answer | Equivalent |
| --- | --- | --- | --- |
| `Status` | `ws_id`, `paths`, `untracked`, `ignored` | `WsStatusResponse` (staged, unstaged, untracked, conflicts, ahead/behind, `in_progress`, `token`) | `asc status --json --untracked-files=all\|no -- <paths>` |
| `Diff` | `ws_id`, `from`, `to`, `paths`, `format`, `context` | stream of `DiffResponse` | `asc diff --json <format flag> [-U<n>] [<from> [<to>]\|--cached <from>] -- <paths>` |
| `Log` | `ws_id`, `query` (`LogRequest`) | stream of `LogResponse` (pages of up to 200 entries) | `asc log --json -n<limit> <include…> ^<exclude…> -- <paths>` |
| `Blame` | `ws_id`, `rev`, `path`, `start_line`, `end_line` | stream of `BlameResponse` (an empty `origin` is an uncommitted line) | `asc blame --json [-L<s>,<e>] [<rev>] -- <path>` |
| `Affected` | `ws_id`, `query` (`AffectedRequest`), `uncommitted` | `WsAffectedResponse` | `asc affected --json [--base …] [--uncommitted] …` (SPEC §12A.9 document) |
| `Stage` | `ws_id`, `add`, `remove`, `patch` | `WsStageResponse` (the new status) | `asc restore --staged -- <remove>`, then `asc add -A -- <add>`, then `asc apply --cached -` (the patch) |
| `Watch` | `ws_id`, `since_token` | stream of `WatchEvent` | `asc watch [--since <token>] --json` |

Details that are not obvious from the proto:

- **`Status`.** `token` is a `Watch` token taken before the status ran: `Watch(since_token = token)` sees
  everything that changed after the status was computed. `ignored = true` is refused, because the JSON
  status has no ignored section. `untracked = false` omits untracked files (the proto default).
- **`Diff` sides.** An empty `from` is `index`, an empty `to` is `worktree`. `index`→`worktree` is the
  unstaged diff; `<rev>`→`worktree` is the work tree against a revision; `<rev>`→`index` is the staged
  diff against a revision (`--cached`); `<rev>`→`<rev>` is two revisions. Other combinations are refused.
  `format` picks `--name-only`, `--name-status`, `--stat`, `--numstat`, `-p` or `--raw`.
- **`Log`.** `limit` 0 means 1,000; at most 100,000. `count_only` and the paging tokens are not served
  here (the workspace log is a stream; use `limit` and `skip`). `repo` is ignored (the workspace's
  repository is used).
- **`Affected`.** The CLI prints the §12A.9 document; the RPC answers the same data as an
  `AffectedResponse` (`projects[].project` carries `id`, `ecosystem`, `name`, `root` and `kind`; `deps`,
  `tags` and `manifest` are not filled; `changed_paths` is 0).
- **`Stage`.** `remove`, then `add`, then `patch` run in that order under the workspace's work-tree gate.
  `add` also stages deletions. A patch that does not apply fails the call and stages nothing. A request
  message is at most 4 MiB (gRPC's default), so a patch is smaller than that.

The stack and history additions (`Stacks`, `Worktrees`, `Conflicts`, `CommitGraph`, and `buffer` and `ignore_whitespace` on `Blame`) are in [`plugin-api-stack.md`](plugin-api-stack.md).

## `Watch`

`Watch` streams `WatchEvent`s for one workspace, push-based; a plugin needs no timer and no file watcher.
It is built from state the daemon keeps anyway, so it resumes without loss.

| Kind | Meaning | Fields |
| --- | --- | --- |
| `KIND_PATHS` | Paths changed in the work tree (the journal) | `paths`, at most 5,000 unique per event |
| `KIND_HEAD` | The checked-out ref moved, or HEAD moved by a switch | `ref`, `old`, `new` (git commit ids) |
| `KIND_REF` | Another ref moved (`ref_log`) | `ref`, `old`, `new` |
| `KIND_STAGE` | The staging area changed | none; ask `Status` |
| `KIND_OP_STARTED`, `KIND_OP_FINISHED` | An operation of the op log | `op` (its kind) |
| `KIND_SWITCH_DONE` | The workspace switched to another ref | `ref`, `op = "switch"` |
| `KIND_RESYNC` | Hints were lost: run `Status` again | none |

- **Tokens.** Every event carries `token`: the position that resumes right after it. A token is opaque
  (`v1.` followed by the journal epoch and sequence, the `ref_log` and `op_log` sequences and a print of
  HEAD and of the staging area). `since_token` empty starts at the present. A token that is malformed is
  `INVALID_ARGUMENT`; one the daemon cannot honour (its logs were reset, or hints were lost) gives a
  `KIND_RESYNC` first and then continues from the present.
- **Resume.** `asc watch --since <token of the last event>` receives every ref, HEAD, operation and
  staging change after it, once. Path hints are kept only while someone watches: edits made while no
  stream was open cannot be replayed, so a resumed stream that had to start watching afresh begins with
  `KIND_RESYNC`.
- **Order.** Within one poll the daemon emits paths, ref and HEAD events, the switch, operations, then
  staging. Events are coalesced per poll (about 200 ms), not per file.
- **Plain workspaces.** There is no provider to journal edits, so while a stream is open the daemon scans
  the work tree (metadata only, ignore rules applied, links never followed), at most every 250 ms and
  never more than a third of the time. A tree of more than 200,000 files is not scanned; the stream says
  `KIND_RESYNC` once and sends no path events. A virtual workspace's provider journals every write itself.
- **Operations.** `KIND_OP_STARTED` is sent right before `KIND_OP_FINISHED`: the op log records an
  operation when it ends. A plugin should treat the pair as one "operation finished" notification.
- **The stream ends** when the workspace is unmounted or the daemon stops; reconnect with the last token.

`asc watch [--since <token>] [--json]` prints the same events, one per line (JSON lines in the asc JSON
profile with `--json`, else `KIND <ref> <op> <paths>⇥<token>`). It starts the daemon and mounts the
workspace if needed, so it fails when `ASC_NO_DAEMON` is set.

## Testing

`ASC_DAEMON_ENDPOINT` names another daemon endpoint (a pipe name on Windows, a socket path elsewhere) for
every asc process that sees it. The tests in `crates/asc/tests/plugin_api.rs` run a private daemon with it
and compare each RPC with the `--json` command.
