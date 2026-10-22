# Built-in MCP for asc

Status: design for the checklist decision "MCP built in" (2026-10-22, `scion-checklist.md`, last sections). This file is not normative until it is folded into SPEC.md and PLAN.md. §9 lists the edits those files need.

Inputs: the MCP specification 2026-07-28 (stateless protocol, `server/discover`, MRTR, `subscriptions/listen`, SEP-2243 headers, OAuth 2.1 with RFC 9728), rmcp 3.6.0, and the GitHub, GitLab, Gitea and Forgejo MCP servers (research of 2026-10-22). Background: SPEC §7 (RPC), §9.1 (daemon IPC), §11.2 (`--json`), §12A.8 (Projects), §13 (Forgejo), §15 (security), §16.3 (rate classes), §20 (stacks), `stacks.md` D1–D12, and `CARRY.md`.

Fixed rules this design obeys:
- git commands keep git's semantics, output and exit codes; MCP is a new surface and changes none of them;
- the default branch is `master`;
- there is no IDE git plumbing and no LFS;
- efficiency is core: bounded memory, no whole-repo scans, everything served from derived data;
- local-only mode keeps working (`asc mcp` with no server and no daemon).

---

## 0. Decisions

| # | Decision | Why |
|---|---|---|
| M-D1 | **Three surfaces with one owner per fact.** `asc mcp` (stdio) serves the workspace and the repo. `asc serve` `/mcp` (Streamable HTTP) serves the repo without a checkout. The Forgejo fork's `/api/mcp` serves social and CI state only (PRs, reviews, issues, Actions, inbox). Forgejo exposes no file, tree, commit, diff, blame, branch or tag tool. asc exposes no PR, review, issue or Actions tool. | Same split as stacks D9: asc owns forge-neutral change facts, Forgejo owns social state. No tool exists twice with two meanings. |
| M-D2 | **Proto is the single schema source.** Tool input and output schemas are JSON Schema 2020-12, generated at build time from the `asc.v1` descriptor set. CLI `--json` output and MCP use one codec, `asc_rpc::json` (§3.2), the versioned "asc JSON profile" (`json.profile.v1`). `asc api` keeps standard proto3 JSON, as SPEC §7 promises, and is not affected. asc-only verbs that have no RPC get messages in a new `proto/asc/v1/cli.proto`. Every `structuredContent` is a JSON object, so a bare-array CLI shape is wrapped in a message (`asc op log --json` prints `{"ops": [...]}`). The C07 shapes are reconciled with these schemas before M02 (§8). | SPEC §11.2 says `--json` is "the proto3 JSON of the matching `Ws*Response`". serde plus schemars would give a second source that drifts from the RPCs. The profile changes casing, ids and enums, so it is versioned and kept off `asc api`. |
| M-D3 | **Stateless, handle-based.** There are no MCP sessions (2026-07-28). Every call names its target: `repo` (HTTP), the workspace (stdio, implicit from the launch directory, overridable with `worktree`), and revisions or change ids as ordinary arguments. Change ids and changeset ids are the handles. | Fits the spec's "explicit handles" guidance and branchless asc: a change needs no branch, and its id is stable across rewrites (SPEC §20.2). |
| M-D4 | **Read-only by default, and only the user can widen it.** Write tools are not listed unless config enables them: `asc.mcp.write = local` for verbs with no network effect, and `remote` adds the verbs that upload or contact the network (§2.3 defines the split). The keys `asc.mcp.write` and `asc.mcp.confirm` are honoured only from user and system config, never from repo or worktree config. Flags (including those in a committed `.mcp.json`) can only narrow access below that ceiling: `--read-only`, a lower `--write`, fewer toolsets. On HTTP, writes also need the `asc:write` scope and `[mcp] write = true`. `--read-only` always wins. | A repository must not be able to switch on writes or switch off confirmation for the agent that opens it. GitHub, Gitea and the Forgejo community servers all ship a read-only mode. Remote effects (submit, land) cannot be undone (SPEC §20.3). |
| M-D5 | **Destructive confirmation uses MRTR elicitation** (form mode) with a sealed `requestState` (rmcp `request-state`: HMAC, principal, 5 min TTL, request digest). A client without elicitation gets an `isError` result; only the user's own user or system config (`asc.mcp.confirm = never`, M-D4) skips the confirmation. Summaries are rendered from escaped, truncated fields (§4). | A model-controlled `confirm: true` argument is not a confirmation. |
| M-D6 | **Every result is bounded.** A token budget (`max_tokens`, default 6,000, hard cap 25,000, the Claude Code default limit; estimated at 3 bytes per token, §5.2) applies to every call, with structure-preserving truncation and an opaque `next_cursor`. Streams are consumed under the budget and dropped (cancelled) when it runs out. | Agents pay per token; a 400k-file repo must not reach the model. |
| M-D7 | **Reads go through the existing RPCs. Local writes run the CLI verb as a child `asc <verb> --json`.** The child is `std::env::current_exe()`, never `asc` from `PATH` (which may be the git shim). Its stdin is null and its stdout is captured (§4). | Reads reuse derived data with no new code path. A child process reuses the hooks, op log, ref chokepoint (SPEC §9.4.1) and locks exactly and bounds memory per write. Writes are rare, so a process spawn costs nothing that matters. Cancellation is graceful first (§4). |
| M-D8 | **MCP inherits the branchless setting.** No tool creates, lists or switches branches, and `create` has no `branch` argument. `create` passes no branch flag: under branchless mode (the default, per repo or per user) the CLI default is `--no-branch`, and when a team or user has switched branchless off, the CLI names a branch from `asc.stack.branchTemplate` as it does for a human. The SPEC §20.7 edit that makes `--no-branch` the default under branchless mode is listed in §9. Navigation and stacks use change ids. | This follows the checklist decision "branchless by default", including its per-repo and per-user opt-out. Git branch UX stays in the CLI. The server `commit` tool cannot move branches either (§2.3). |
| M-D9 | **A small default toolset.** The default `core` set has 11 read tools. Other sets (`history`, `stack`, `projects`, `search`, `write`) are opt-in through `--toolsets`/`asc.mcp.toolsets`. `tools/list` order is fixed by the manifest, so prompt caches hit. | GitHub PR #1159: the default set was cut 53% by usage, with no loss of top-20 tools. |
| M-D10 | **rmcp 3.6.0 without macros.** Tools are table-driven from the generated schemas: a manual `ServerHandler`, with no `#[tool]` and no schemars. | Schemas come from proto (M-D2), and macros would derive them from Rust types instead. |

---

## 1. Surfaces and placement

| Surface | Transport | Process | Auth | Backend | Scope |
|---|---|---|---|---|---|
| `asc mcp` | stdio (newline-delimited JSON-RPC), logs to stderr | the `asc` binary, launched by the agent host in a workspace | none on the pipe; repo calls use the user's existing credentials (`ASC_TOKEN`, else `git credential fill`, SPEC §15.1). No OAuth on stdio | daemon IPC (`Workspace`; `asc mcp` starts the daemon on first need, as the CLI does), in-process only when `ASC_NO_DAEMON` is set (plain workspaces only, SPEC §9.1); the server's `Repo`/`Changes`/`Projects` when the remote is an asc server (same L/S routing as SPEC §11.2), else the local engine | one workspace and its repo |
| `asc serve` `/mcp` | Streamable HTTP 2026-07-28 (POST only; GET and DELETE return 405) | in `asc serve` (primary role only; the cache role returns 404) | OAuth 2.1 resource server (RFC 9728) with Forgejo as the authorization server, or a Forgejo PAT as the bearer token | in-process `RepoHandlers`, `Changes` and `Projects` (no network hop) | any repo the principal can read, named per call |
| Forgejo `/api/mcp` | Streamable HTTP 2026-07-28, stateless | in the Forgejo fork (Go), calling Forgejo services in-process (not REST) | Forgejo's API auth (PAT or OAuth2 token) and its scopes | Forgejo models and services | PRs, reviews, issues, Actions, inbox |

**Placement rule.** If the answer comes from a changeset, tree, file, ref or derived data, the tool is asc's. If the answer comes from Forgejo's database (people, discussion, approvals, queue, runs), the tool is Forgejo's. Forgejo results carry the asc handles (`asc_change`, `head`, `merge_base`), so the agent calls asc tools for the code.

| Need | Tool | Surface |
|---|---|---|
| PR diff, interdiff, versions, stack shape, mergeability | `diff`, `interdiff`, `change_versions`, `stack`, `change_info` | asc |
| PR metadata, reviews, comments, checks summary, merge, queue | `pr_read`, `pr_write`, `review_write` | Forgejo |
| Create or update a PR | `submit` (an AGit push of the change, SPEC §20.5 flow 1) | asc |
| Merge or queue a stack | `pr_write {method: merge \| auto_merge}` (stack-aware, C36/C38/C39) | Forgejo |
| Issues | `issue_read`, `issue_write` | Forgejo |
| Actions runs, jobs, logs | `actions_read`, `actions_write` | Forgejo |
| Inbox (whose turn) | `inbox_read`, `inbox_write` | Forgejo |
| Affected projects of a PR | `affected` (with `base` = `asc_stack_parent_sha`) | asc |

Which server to configure: a local agent uses `asc mcp` plus Forgejo `/api/mcp`. A remote agent with no checkout uses `asc serve` `/mcp` plus Forgejo. `asc mcp install` writes the first pair into `.mcp.json` (project) or the user config, and never adds the asc HTTP endpoint next to `asc mcp`, so an agent never sees the same tool twice.

---

## 2. Tool catalogue

### 2.1 Conventions

- **Names** are short, without a prefix: hosts already namespace them by server (`mcp__asc__log`). The pattern is `[a-z_]+`.
- **Input** is the RPC request message, projected:
  - injected fields are removed (`repo` on stdio, `ws_id`);
  - plumbing fields are hidden (`null_terminated`, `binary`, `id_mode`, `count_only`);
  - MCP-only fields are added: `max_tokens`, `cursor`, `worktree` (stdio), and the convenience fields listed per tool.
  - `worktree` must equal (after canonicalisation) one of the paths listed by `Workspace.Worktrees` for the launch workspace. Any other value is -32602. It is a selector, not a path to open.
  
  On HTTP, `repo` (`"owner/name"`) is required and carries `x-mcp-header: "Repo"`, so gateways can route and rate-limit on `Mcp-Param-Repo`.
- **Revision arguments** accept a git revspec, a full or abbreviated git SHA, an asc hex id, a change id prefix (at least 4 letters `k`–`z`) and `asc:<snapshot>`. They resolve in SPEC §20.2 order.
- **Output** (`structuredContent`) is the response message in the asc JSON profile (§3.2). A streamed RPC is folded into one message (proto merge: repeated fields concatenated). Two optional top-level fields are added: `truncated` (where the message lacks one) and `next_cursor`. The result is always a JSON object: a CLI shape that is a bare array is wrapped by its `cli.proto` message. `outputSchema` is generated, and the result always conforms.
- **Text block** is the git-format rendering, not a second copy of the JSON: a patch for `diff`, `--oneline` for `log`, porcelain v2 for `status`, `path:line:text` for `grep`. `asc.mcp.textMode = json` switches it to serialized JSON for clients that ignore `structuredContent`.
- **Annotations:** `R` = `readOnlyHint: true`. For writes, `d`/`i` = `destructiveHint`/`idempotentHint` true (otherwise explicitly false). Annotations are static per tool and cannot depend on arguments, so a tool that is destructive for some arguments is marked `d`. `openWorldHint` is false on asc tools that stay inside the closed system of server, cache and daemon, and true on `submit` and `sync` (they reach the network) and on any verb §2.3 classes as remote. `destructiveHint: true` means "not reversible with `undo`" or "may drop work".
- **Errors:**
  - an unknown tool or invalid arguments is JSON-RPC `-32602`;
  - everything else is `isError: true` with `structuredContent.error = {code, message, paths, conflicts, retry_after_ms}`, where `code` is `ErrorDetail.code` (SPEC §7) or `NEEDS_CONFIRMATION`, `BUDGET`, `NOT_AVAILABLE`, `STALE_OP`;
  - a child verb's exit code 1 (paused on a conflict) or 2 (ambiguous, with `candidates`) is not an error: the result is `isError: false` with `OpResult.status = "paused"` (and `conflicts`) or `"ambiguous"` (and `candidates`). Exit codes 128 and 129 are `isError`.

### 2.2 Read tools

Surface: L = `asc mcp`, S = `asc serve /mcp`. "L→S" means stdio routes to the server when one is configured and to the local engine otherwise.

| Tool | Set | Surf. | Backing call | Input (projection) | Output | Ann. | Default / max |
|---|---|---|---|---|---|---|---|
| `status` | core | L | `Workspace.Status` | `WsStatusRequest` − `ws_id`; `untracked` default true, `ignored` refused | `WsStatusResponse` | R | 500 files / 5,000 |
| `diff` | core | L | `Workspace.Diff` | `WsDiffRequest` − `ws_id` (`from`/`to`: `index` \| `worktree` \| rev; default `HEAD`→`worktree`) | `DiffResponse` | R | `patch`, 6k tokens; `max_files` 300 (default and cap) |
| `diff` | core | S | `Repo.Diff` | `DiffRequest` − `null_terminated`, `binary` | `DiffResponse` | R | `max_files` 300 (default and cap) |
| `log` | core | L→S, S | `Workspace.Log` / `Repo.Log` | `LogRequest` (`include`, `exclude`, `paths`, `follow`, `first_parent`, `author`, `grep`, `since`, `until`, `pickaxe`, `with_stats`, `page_token`) | `LogResponse` | R | 20 / 200; scan budget 10,000 commits per page (hard cap 50,000) |
| `show` | core | L→S, S | `Repo.ReadChangeset` | `ReadChangesetRequest` (`rev`) | `ReadChangesetResponse` − `raw_git_commit`, `extra["git.*"]` | R | changes 300 |
| `read_file` | core | L, S | `Repo.ReadFile`; L with no `rev` = working copy | `ReadFileRequest` (`rev_path` split into `rev` + `path`, `offset`, `length`) + `start_line`, `end_line` | `ReadFileResponse` + `start_line`, `end_line`, `total_lines?`, `binary` | R | 400 lines, 256 KiB |
| `ls_tree` | core | L→S, S | `Repo.ReadTree` | `ReadTreeRequest` (`rev`, `path`, `recursive`, `limit`, `after`) | `ReadTreeResponse` + `next_cursor` | R | 200 / 1,000 |
| `grep` | core | L→S, S | `Repo.Grep` (clean files); local for dirty files (SPEC §11.2), through the path gate (§4) | `GrepRequest` (`rev`, `pattern`, `fixed`, `ignore_case`, `paths`, `max_results`) | `GrepResponse` | R | 100 / 1,000; scan budget 5,000 files and 64 MiB |
| `blame` | core | L, S | `Workspace.Blame` / `Repo.Blame` | `WsBlameRequest` − `ws_id` (empty `rev` = working copy) / `BlameRequest` | `BlameResponse` | R | lines 1–200 / 2,000 |
| `smartlog` | core | L | `Workspace.Stacks` (= `asc sl --json`, C31) | `{remote: bool, all: bool}` | `WsStacksResponse` | R | 50 changes |
| `change_info` | core | L→S, S | `Changes.ResolveChanges` | `ResolveChangesRequest` (`changes`, `revs`) | `ResolveChangesResponse` | R | 50 |
| `affected` | core | L, S | `Workspace.Affected` / `Projects.Affected` | `AffectedRequest` (+ `uncommitted` on L) | `AffectedResponse` | R | projects 500 |
| `line_history` | history | L→S, S | `LineHistory` (C30a) | `LineHistoryRequest` (`rev`, `path`, `start`, `end`, `limit`, `scan_budget`, `page_token`) | `LogResponse` | R | 20; budget 10,000 |
| `interdiff` | history | L→S, S | `Changes.InterDiff` | `InterDiffRequest` | `DiffResponse` | R | 6k tokens |
| `compare` | history | L→S, S | `Compare` (C30b) | `CompareRequest` (`a`, `b`, `three_dot`, `limit`) | `CompareResponse` | R | 50 per side |
| `stack` | stack | L→S, S | `Changes.Stack` | `StackRequest` | `StackResponse` | R | 100 |
| `change_versions` | stack | L→S, S | `Changes.Versions` | `VersionsRequest` | `VersionsResponse` | R | 20 |
| `op_log` | stack | L | child `asc op log --json -n` | `OpLogArgs {limit}` (cli.proto) | `OpLogResponse` (cli.proto; argv already redacted, SPEC §20.3) | R | 20 / 200 |
| `conflicts` | stack | L | `Workspace.Conflicts` | `{paths}` | `WsConflictsResponse` | R | 100 |
| `worktrees` | stack | L | `Workspace.Worktrees` | `{}` | `WsWorktreesResponse` | R | 100 |
| `project_graph` | projects | L→S, S | `Projects.ProjectGraph` | `ProjectGraphRequest` (`rev`, `types`, `select`) | `ProjectGraphResponse` | R | 500 projects |
| `search` | search | L→S, S | code-intel `Search` RPC (pending; listed only when `Version.features` has `search.v1`) | `{query, mode: text\|regex\|symbol\|semantic, rev, paths, limit}` | `SearchResponse` | R | 50 / 200 |

Notes:
- `read_file` with `start_line`/`end_line` streams `ReadFile` from offset 0 on the first call and stops after `end_line`, so memory is bounded by the budget, not by the file. Its `next_cursor` carries a byte offset (and the line number at it), so a continuation seeks to that offset and costs O(page), not a restream from 0 (§5.2). Binary content (by attributes or a NUL in the first 8 KiB) returns metadata only (`binary: true`, size, id). In L mode with no `rev`:
  - a clean tracked path is read from the object cache by `FileId`, so a virtual placeholder is never hydrated;
  - a dirty or untracked path is read from disk, only through the path gate (§4): canonicalised, inside the worktree root, symlinks not followed (a symlink is reported as a link, with its target text, never read through);
  - an ignored path is refused, so `.env` files are never served. The same single ignore gate applies to `grep` (dirty files), `blame` (working copy), `stage` and the `asc://ws/…/file` resource.
- `ls_tree` with `recursive: true` needs a non-root `path`. A root-recursive call returns `isError` with code `BUDGET` and suggests `grep`, `search` or a subdirectory.
- `blame` and `line_history` without a line range use 1–200.
- `diff` keeps the `changes` list (paths and statuses) up to `max_files` = 300 and cuts patch text at hunk boundaries (§5.2). Files beyond 300 are counted in `omitted.files`, not listed; the cursor continues with the next 300.

### 2.3 Write tools (toolset `write`; listed only when enabled, M-D4)

Local writes run `asc <verb> --json` as a child process (M-D7; §4 covers spawning, the stdio channel and cancellation). The child gets `GIT_TERMINAL_PROMPT=0` and `GIT_EDITOR=false` (no editor; a message must be passed), and inherits the user's environment otherwise, so hooks run as they would for `git commit`. Inputs are `*Args` messages in `cli.proto` that mirror the CLI flags. Outputs are `OpResult` (the SPEC §20.7 "every mutating verb" shape).

**What "local" means.** `write=local` lists only the verbs that never touch the network: `stage` and `worktree_create`. The verbs that create or rewrite changes (`create`, `modify`, `absorb`, `restack`, `undo`, `redo`) are local only while the workspace does not upload: local-only mode, no reachable server, or branchless upload switched off. When the client is online in branchless mode, those verbs upload the resulting drafts at once through the daemon (checklist, "branchless by default"), so they need `write=remote` and carry `openWorldHint: true`. The catalogue computes this per workspace when it builds `tools/list`. `sync` and `submit` always need `remote`.

| Tool | Surf. | Backing call | Input | Ann. | Confirm (elicitation) | Needs |
|---|---|---|---|---|---|---|
| `stage` | L | `Workspace.Stage` | `WsStageRequest` − `ws_id`, `force` (hidden; ignored paths are never staged) | i | – | `write=local` |
| `create` | L | `asc create --json` | `CreateArgs {message (required), all, onto?, insert}`; no branch flag (M-D8) | – | – | local or remote (above) |
| `modify` | L | `asc modify --json` | `ModifyArgs {message?, all, into?, new_change}` | – | – | local or remote (above) |
| `absorb` | L | `asc absorb --json` | `AbsorbArgs {apply (default false = preview), into?}` | – | – | local or remote (above) |
| `restack` | L | `asc restack --json` | `RestackArgs {change?, scope: all\|downstack\|upstack\|only}` | i | – | local or remote (above) |
| `undo` | L | `asc undo --json` | `UndoArgs {op (required), dry_run}`; `op` is the id from the agent's earlier `OpResult`; refused with `STALE_OP` unless that op is the latest in the op log | d | per `asc.mcp.confirm` | local or remote (above) |
| `redo` | L | `asc redo --json` | `UndoArgs {op (required), dry_run}`; `op` must be the latest undone op | – | – | local or remote (above) |
| `worktree_create` | L | `asc worktree add --json` | `WorktreeAddArgs {path, rev, ephemeral, ttl, mode}` | – | – | `write=local` |
| `sync` | L | `asc sync --json` | `SyncArgs {fetch (true), restack (true), delete_merged (false)}` | open-world | when `delete_merged` | `write=remote` |
| `submit` | L | `asc submit --json` | `SubmitArgs {change?, whole_stack, draft (default `asc.stack.submit.draft`, else true), reviewers, update_only, force}` | d, open-world | always | `write=remote` |
| `commit` | S | `Repo.Commit` | `CommitRequest` − `author`, `committer`, `sign` (identity = the principal). `update_ref` must be empty (the changeset is ingested as a draft change, SPEC §8.7) or `refs/for/*`; any other value, including `master` and every branch, is -32602 | – | when `update_ref` is `refs/for/*` | `asc:write`, `[mcp] write` |
| `submit` | S | `UpdateRefs` of `refs/for/<trunk>/asc-<change>` (SPEC §20.5 flow 1) | `{changes, draft, reviewers, force}` | d | always | `asc:write` |
| `rebase` | S | `Repo.Rebase` (SPEC §8.11, agent conflict API) | `RebaseRequest` (`dry_run` default true); `update_ref` follows the same rule as `commit` | – | when `update_ref` set and not dry run | `asc:write` |
| `restack` | S | `Changes.RestackChanges` | `RestackChangesRequest` − `committer_*` (`dry_run` default true) | – | – | `asc:write` |

- `undo` is marked destructive because annotations are static (§2.1). It takes the explicit `op`, so an agent can revert only the operation it just made: a later operation by the user makes `undo` fail with `STALE_OP`. `redo` is a separate, non-destructive tool for the same reason.
- A server `commit` or `rebase` with `refs/for/*` runs the SPEC §8.3 `UpdateRefs` pipeline, so protections, hooks, the Forgejo push event and Actions triggers fire exactly as for a push. An empty `update_ref` runs only pipeline step 4 (SPEC §8.7). Server `commit` writes a `ChangeMeta` for the changeset it creates (the SPEC §8.7 gap; unit M18, which M14 needs).

Not exposed in v1: `abandon`, `split`, `squash`, `fold`, `move`, `reorder`, `reword` and every git command (the agent's shell has `git`/`asc`). Landing is Forgejo's `pr_write {method: merge}`, so policy and the queue stay in one place.

### 2.4 Forgejo tools (`/api/mcp`)

These are consolidated read/write tools with a `method` parameter, following GitHub and gitea-mcp. Every result includes the asc handles.

| Tool | Methods | Ann. |
|---|---|---|
| `pr_read` | `get` (includes `asc_change`, `head`, `merge_base`, `stack_parent`, `version`), `list`, `search`, `reviews`, `review_comments`, `comments`, `checks`, `stack` (the panel: PR numbers bottom to top) | R |
| `pr_write` | `update` (title, body), `set_draft`, `request_reviewers`, `close`, `reopen`, `comment`, `merge` (stack prefix; 409 `stack-parent-unmerged` is returned as `isError`), `auto_merge`, `cancel_auto_merge` | `merge`: d, confirm |
| `review_write` | `create_pending`, `add_comment`, `submit` (`approve` \| `request_changes` \| `comment`), `dismiss` | openWorld true (it notifies people); confirm on `submit` with `approve`, and on `dismiss` |
| `issue_read` | `get`, `list`, `search`, `comments`, `labels` | R |
| `issue_write` | `create`, `update`, `comment`, `close`, `reopen`, `label` | openWorld true; confirm on every method except `label` |
| `actions_read` | `runs` (by PR, change or commit), `run`, `jobs`, `job_log` (`tail_lines` default 200; `start_line`/`end_line`; 64 KiB cap), `artifacts` (metadata only) | R |
| `actions_write` | `rerun` (all or failed jobs), `cancel`, `dispatch` | – |
| `inbox_read` | `list` (sections from C41b: needs review, returned, ready, blocked, waiting, drafts, queued) | R |
| `inbox_write` | `pin`, `unpin`, `snooze` | i |

The Forgejo endpoint is read-only unless `[mcp] WRITE = true`, and the token's scopes still apply (`read:issue`, `write:repository`, and so on). Write methods are also refused (`NOT_AVAILABLE`) until the `lockdown` mode (§4, unit M28) is available, so a build without it cannot enable them.

**Handle fields.** `asc_change`, `head`, `merge_base`, `stack_parent` and `version` are one `AscHandles` message in `cli.proto` (M02). The asc-mcp build also writes it as a JSON Schema file, `crates/asc-mcp/schema/asc_handles.schema.json`, which M21 vendors into `routers/api/mcp/schema/` with a recorded hash; the Forgejo tools embed it in their `outputSchema`. M17 drift-checks the hash.

### 2.5 Resources

The scheme is `asc://`. `resources/list` never enumerates repository content. It returns only the workspace's `status` and `smartlog` (L). Everything else comes from templates (RFC 6570, with argument completion for `rev` and `path` from `ReadTree`, limited to 50 entries).

| URI template | Content | `ttlMs` | `cacheScope` |
|---|---|---|---|
| `asc://{owner}/{name}/blob/{rev}/{+path}{?start_line,end_line}` | file text (`read_file` rules, including binary refusal) | 86,400,000 if `rev` is a full id, else 5,000 | private |
| `asc://{owner}/{name}/tree/{rev}/{+path}{?after}` | one `ls_tree` page (non-recursive) | as above | private |
| `asc://{owner}/{name}/commit/{rev}` | `show` | as above | private |
| `asc://{owner}/{name}/change/{change}` | `change_info` | 5,000 | private |
| `asc://ws/{ws}/status`, `asc://ws/{ws}/smartlog` (L) | `status` / `smartlog` | 0 | private |
| `asc://ws/{ws}/file/{+path}` (L) | working-copy file (tracked, or untracked and not ignored), through the path gate (§4) | 0 | private |

Local-only repos use `owner = local` and `name` = the repo directory name. Resource annotations set `audience: ["assistant"]`.

**Subscriptions (`subscriptions/listen`, L only in v1).** `resourceSubscriptions` on the `asc://ws/…` URIs are served from `Workspace.Watch`, including `KIND_STACK` (C49). Paths and HEAD events become `notifications/resources/updated`. A process has at most 2 listens with 64 URIs each. HTTP subscriptions (ref moves on the server) are v1.x. `toolsListChanged` fires when the server later advertises a feature, for example `search.v1`.

### 2.6 Prompts

Each prompt is a short template that names the tools to call. None embeds repository content.

| Prompt | Arguments | Does |
|---|---|---|
| `review_change` | `change` | `change_info` → `diff` (vs the parent) → `affected` → summarise risk by `Classify` class |
| `resolve_conflicts` | – | `conflicts` → `read_file` per side → edit on disk → `stage` → `restack` |
| `prepare_submit` | `change?` | `smartlog` → restack if needed → `affected` → `submit` (confirmed) |
| `explain_lines` | `path`, `start`, `end` | `blame` → `line_history` → `show` of the origin commits |
| `affected_tests` | `base?` | `affected --target test` → the commands to run |

---

## 3. Schemas and codec

### 3.1 Generation

- U41's `crates/asc-rpc/build.rs` writes the `FileDescriptorSet` (`tonic-prost-build` `file_descriptor_set_path`) and exports `asc_rpc::DESCRIPTOR_SET` (an amendment, §9).
- `crates/asc-mcp/build.rs` reads `tools.toml` (one row per tool: name, toolset, surfaces, RPC, request/response message, hidden fields, added fields, defaults, caps, annotations, confirm rule, description). Using `prost-reflect` (already pinned in U01b), it emits `tools_gen.rs`: a static table with each tool's inputSchema and outputSchema as `&'static str` JSON.
- Field mapping:
  - `repeated` → array;
  - `oneof` → `oneOf`;
  - enums → string enums (§3.2);
  - 64-bit integers → `integer`;
  - messages → `$defs` with `$ref`, one `$defs` per schema, so inputSchema stays self-contained.
- Descriptions come from proto comments (the same source that produces `docs/api.md`, SPEC §7).
- The same build writes `crates/asc-mcp/schema/asc_handles.schema.json` from `AscHandles` (shared with the Forgejo tools, §2.4).
- A snapshot test (insta) freezes `tools/list` for each toolset. The default set must estimate at most 6,000 tokens.

### 3.2 The asc JSON profile (`asc_rpc::json`, used by `--json` and MCP)

| Proto | JSON | Driven by |
|---|---|---|
| field names | `snake_case` (proto names) | profile default (matches SPEC §20.7 shapes) |
| `bytes` id (20 or 32 bytes) | lowercase hex (40 = git SHA, 64 = asc id) | field option `(asc.v1.json) = HEX` |
| `bytes` change id | 32 letters `k`–`z` (SPEC §20.2) | `CHANGE` |
| `bytes` path, message, text | UTF-8 string; non-UTF-8 → `{"b64": "…"}` | `PATH` / `TEXT` |
| `bytes` file data | as `TEXT`, plus `binary: true` and no data when binary | `DATA` |
| enums | lowercase value without the type prefix (`DIFF_FORMAT_PATCH` → `"patch"`); input also accepts the full name | profile |
| `int64`/`uint64` | JSON number (all asc values are below 2^53) | profile |
| `Signature` | `{name, email, time, tz}` with `tz` as `+0530`, computed from the proto's `tz_min` (the SPEC §20.7 example gains `tz`, §9) | profile |

The option lives in a new `proto/asc/v1/options.proto` (`extend google.protobuf.FieldOptions { JsonKind json = 50101; }`). The annotations on existing fields are a recorded amendment to U41's proto files. Decoding uses the same table, so tool inputs accept exactly what outputs print.

**Versioning.** The profile is named `json.profile.v1` and is advertised in `Version.features` and in `server/discover`. `asc api` is not affected and keeps standard proto3 JSON (SPEC §7). An incompatible profile change creates `v2` and `v1` stays for one major version. Before M02, the C07 `--json` types and the §20.7 shapes are reconciled to these schemas (casing, `tz`, the `op log` wrapper).

---

## 4. Safety

| Concern | Rule |
|---|---|
| Default | Read-only. The write toolset is absent from `tools/list` unless `asc.mcp.write` is set in user or system config (stdio) or `[mcp] write = true` plus the `asc:write` scope (HTTP). Repo and worktree config cannot set `asc.mcp.write` or `asc.mcp.confirm`, and flags (a committed `.mcp.json` can carry any) can only narrow (M-D4). `--read-only` overrides everything. The Forgejo endpoint uses `[mcp] WRITE`. |
| Path escape (stdio) | `worktree` is accepted only if it is a `Workspace.Worktrees` entry. Every path argument passes one path gate: it is canonicalised and refused if it is absolute, has `..`, a drive or UNC prefix, or resolves outside the worktree root; each component is checked without following symlinks (open with `O_NOFOLLOW`, or `FILE_FLAG_OPEN_REPARSE_POINT` on Windows), so a symlink or junction is reported as a link and never read through. Then the same ignore gate (`.gitignore`/`.ascignore`) applies to `read_file`, the `asc://ws/…/file` resource, `grep`, `blame` and `stage`. `stage --force` is not exposed. |
| Protocol channel (stdio) | fd 1 carries only JSON-RPC. At startup `asc mcp` keeps a private handle to the real stdout for the transport and redirects fd 1 (and the Windows standard output handle) to stderr, so stray prints from hooks, the engine or libraries cannot corrupt the stream. Child verbs get stdin null and stdout piped (captured as the `--json` result); their stderr is captured and logged to stderr. In-process code writes to stderr only. A test runs a hook that prints to stdout. |
| Annotations | Every tool sets all four hints explicitly (§2.1). Clients treat them as hints. The server enforces the same gates itself. |
| Confirmation | Tools marked "confirm" return `resultType: input_required` with one `elicitation/create` (form: a boolean `confirm` plus a summary of the effect, for example "submit 3 changes to master: …"). The summary is rendered from escaped, truncated data fields (control characters and markup stripped, each commit subject cut to 80 characters, at most 10 listed, no message bodies), because commit subjects and titles are attacker-controlled. The retry must echo the sealed `requestState`, whose HMAC binds principal, tool, argument digest and a 5 min TTL. A replay with other arguments fails. Without the elicitation capability → `isError` `NEEDS_CONFIRMATION`. `asc.mcp.confirm = always \| destructive (default) \| never` is the user's switch. Elicitation never asks for secrets (form mode). |
| Keys and sealing | Cursors and `requestState` are sealed with HMAC-SHA256. On stdio the key is random per process, so a cursor does not outlive the process (the client gets -32602 and starts again). On HTTP, `[mcp] hmac_keys` lists key files (32 random bytes each) shared by all `asc serve` replicas; a token carries its key id, the first key signs, all keys verify. Rotation: prepend a new key, drop the old one after the longest TTL (1 h for cursors). A missing or unreadable key file stops the endpoint from starting. |
| Authorisation (HTTP) | Every call authorises `(principal, repo, read\|write)` through the SPEC §15.1 cache (60 s positive, 5 s negative). `tools/list` varies only by granted scopes, as the spec allows. A repository the principal cannot read returns the same `NOT_FOUND` (same message, same cost class) as a missing one. |
| Directory ACLs | As SPEC §15.5. Read access is per repository, and a sensitive directory is its own repository composed at a gitlink. `ls_tree` shows an unreadable gitlink as `{restricted: true}`. `read_file`, `blame` and `grep` under it return `isError` `PATH_POLICY`. `grep`/`search` results only include namespaces the principal can read. Write path rules surface as `PATH_POLICY` from pre-receive. |
| Secrets | No tool reads config, environment or credentials. Ignored files are never served. `op_log` argv is redacted (SPEC §20.3). Error text passes through the same redactor (URL credentials, `-c` values, `--token`). Cursors and `requestState` hold ids and offsets, never tokens. Tokens only travel in the `Authorization` header: a query-string token gets 400. stderr logs record tool names and durations, not arguments, unless `ASC_MCP_LOG=args`. |
| HTTP transport | Validate `Origin` when present (allowlist: the server's own origin plus `[mcp] allowed_origins`; otherwise 403). An absent `Origin` is accepted, because non-browser clients send none and the endpoint takes bearer tokens only (no cookies). Validate `MCP-Protocol-Version`, `Mcp-Method` and `Mcp-Name` against the body (-32020 on mismatch). Request body ≤ 1 MiB (4 MiB for `commit`). `X-Accel-Buffering: no` on SSE. |
| Untrusted content | File, PR, issue and log content is data. The server `instructions` say so, and resource annotations mark the audience. Forgejo `lockdown` (hide content by authors without write access, as GitHub `--lockdown-mode`) ships in v1 (unit M28), and Forgejo write methods are unavailable until it exists. Confirmation summaries never include raw untrusted text (see Confirmation). |
| Hooks | `create`/`modify` run the repo's commit hooks, as `git commit` would for the same agent. There is no extra sandbox (SPEC §15.4: the workspace is the user's). |
| Rate limits | Each MCP call is charged to the SPEC §16.3 class of its backing RPC (`log` → cheap or expensive, `diff` patch → expensive). In addition there are at most 8 in-flight MCP calls per principal per `asc serve` instance (not cluster-wide; a gateway enforces a cluster limit on `Mcp-Param-Repo`). `RATE_LIMITED` returns `isError` with `retry_after_ms`. |
| Bounded memory | There are no sessions, so the bounds are per call and per process. Per call: the response buffer is at most `max_tokens × 3` bytes plus 64 KiB, and a stream is read until the budget is spent and then dropped. Per `asc mcp` process: 8 concurrent calls, a queue of 32, then `isError` "busy"; idle RSS ≤ 24 MiB, ≤ 64 MiB under load (bench-gated). Per HTTP principal and instance: 8 in-flight calls and 2 listens of at most 1 h each. No result is cached in asc-mcp; caching stays in the tiers. |
| Cancellation | stdio: `notifications/cancelled` drops the RPC stream. A child verb is cancelled gracefully first: it runs in its own process group (Windows: a job object with a new process group), receives SIGTERM (Windows: `CTRL_BREAK_EVENT`), gets 3 s to finish, and is then killed (the whole group or job). After a kill, lock recovery runs: an `index.lock` or ref lock created after the child started and held by no live process is removed, so turbo's `git status` does not fail on a stale lock. The op log records an aborted op, and `undo` covers it. HTTP: closing the SSE stream drops the stream the same way. Progress: `sync`, `submit`, cold `affected` and `rebase` emit `notifications/progress` when a `progressToken` is set. |

---

## 5. Efficiency

### 5.1 Every tool is served from derived data

| Tool | Data | Bound |
|---|---|---|
| `log`, `line_history`, `compare` | gen numbers, skip lists, `path_changes` (SPEC §5.1), page tokens | O(page + log n); scan budget 10,000 commits per page (hard cap 50,000) on `log`, pickaxe and line history |
| `blame` | `blame.v1` memo | O(file) after the first derive; ranged output |
| `diff`, `interdiff`, `show` | changeset `changes` lists (Bonsai), `leafdiff` with hash pruning | O(changed paths); patch text only for paths inside the budget |
| `ls_tree`, `read_file` | trees and files by id, ranged `ReadFile` | O(page) / O(range) |
| `grep` | server `Grep` at the base (no hydration); local for dirty files only | `max_results`, plus the scan budget of 5,000 files and 64 MiB per call; `search` uses the trigram index when present |
| `affected`, `project_graph` | project graph derived data (SPEC §12A.5) | O(changed manifests) |
| `smartlog`, `stack`, `change_info`, `change_versions` | the local change index / server `changes` and `change_versions` | O(heads × stack length × log n) |
| `status` | the workspace journal | O(changed paths) |

No tool walks a whole tree or the whole history. A request that would need to (root-recursive `ls_tree`, pickaxe without a path, a `log` page beyond its 10,000-commit `scan_budget`) returns `BUDGET` with the bounded alternative.

### 5.2 Token-aware truncation

- **Estimate:** `tokens ≈ ceil(utf8_bytes / 3)`, computed while rendering. There is no tokenizer. Code averages about 3 bytes per token, so `bytes / 4` would overshoot the 25,000-token host limit; `bytes / 3` is the conservative estimate.
- **Budget:** `min(max_tokens argument, asc.mcp.maxTokens (6,000), 25,000)`, shared by `structuredContent` and the text block. In git text mode the text block is the bigger part, and the JSON keeps its structure.
- **Per-shape strategy:**

| Shape | Keep first | Cut |
|---|---|---|
| diff | the file list with statuses and stats, up to 300 files | whole hunks, file by file in path order; omitted files and hunks counted in `omitted: {files, hunks, lines}` |
| file | the requested range | trailing lines; `next_cursor` carries the byte offset and line of the next line |
| log, grep, tree, lists | whole entries | the tail; `next_cursor` |
| blame | whole hunks covering the range | the tail of the range |
| status | conflicts, then staged, then unstaged | untracked paths (counted) |

- **Cursors:** a tool backed by a paged RPC reuses its `page_token`/`after`, wrapped. For other tools the cursor is an HMAC-sealed `{tool, argument digest, position}` (file index, byte offset plus line number; key handling in §4). A cursor from another call returns -32602.

---

## 6. Crate and implementation

### 6.1 Dependencies

`[workspace.dependencies]` gains one line:

```toml
rmcp = { version = "3.6.0", default-features = false, features = ["server", "base64", "transport-io", "transport-async-rw", "transport-streamable-http-server", "elicitation", "request-state"] }
```

- rmcp 3.6.0 (2026-10-20, Apache-2.0, MSRV 1.88, below the workspace's 1.95) implements 2026-07-28 and keeps 2025-11-25 compatibility, so older stdio hosts that still send `initialize` work.
- Not enabled: `macros` and `schemars` (M-D10), `auth` (the server-side resource-server checks are a small axum layer of ours), and every client and reqwest feature.
- Dev-dependencies of the test units add `client` and `transport-child-process`, which give the inspector-style harness.
- M03 checks that rmcp's axum and tower match the workspace pins (`cargo tree -d` shows one axum), and that `cargo deny` passes.

### 6.2 Layout and reuse

```
crates/asc-mcp            protocol layer, no repo logic
  build.rs               tools.toml + asc_rpc::DESCRIPTOR_SET → tools_gen.rs (schemas, table)
  tools.toml             the catalogue (§2), one row per tool
  src/catalogue.rs       toolsets, gating, deterministic tools/list
  src/handler.rs         rmcp ServerHandler: server/discover, tools/*, resources/*, prompts/*, listen
  src/backend.rs         trait McpBackend { async fn call(&self, rpc: Rpc, req: DynamicMessage, budget) -> Stream<DynamicMessage> }
  src/rpc_backend.rs     McpBackend over asc-rpc RpcClient (Repo, Changes, Projects) and the IPC WorkspaceClient (U52)
  src/shape.rs           stream folding, budget, truncation; text blocks come from asc-engine::render (M04a)
  src/cursor.rs, confirm.rs   HMAC cursors; MRTR elicitation with sealed requestState
  src/resources.rs, prompts.rs, config.rs
crates/asc/src/mcp/       `asc mcp`, `asc mcp install`; LocalBackend (routing + child verbs), pathgate.rs
crates/asc-engine/src/render/  porcelain v2 status, --oneline log, blame, grep text (shared by `asc` and `asc-mcp`)
crates/asc-serve/src/mcp/ `/mcp` mount, auth layer, PRM document, ServeBackend over in-process handlers
proto/asc/v1/options.proto, cli.proto
```

- **Dependency direction:** `asc-mcp` → `asc-rpc` (proto types, descriptors, JSON codec, client, IPC) → `asc-engine`; `asc-mcp` also uses `asc-engine::render` directly, so the CLI and MCP share one set of text renderers (M04a) and cannot drift. `asc` and `asc-serve` (feature `mcp`, on by default) depend on `asc-mcp`. `asc-mcp` never depends on `asc`, so the `asc` crate supplies `LocalBackend` through the trait (the `ext.rs` pattern).
- **`LocalBackend` routing:**
  - `Workspace.*` → daemon IPC. If no daemon runs, `asc mcp` starts it, as the CLI does (SPEC §9.1). Only when `ASC_NO_DAEMON` is set does it run in-process (plain workspaces only): the U73/C31 plugin service object is called directly as a trait object, `state.db` is opened per call (never held by the long-lived server), and the schema version is checked on every open. A mismatch returns `NOT_AVAILABLE` ("restart asc mcp") instead of racing a daemon or a migration.
  - `Repo`/`Changes`/`Projects` → the server `RpcClient` when the remote is an asc server and `asc.serverHistory` routes there (U50's `HistorySelector`). Otherwise U42's `EngineBackend` over the local store, so local-only mode works.
  - Change-service tools in local-only mode use the local change index (C02) through the same message types. Tools with no local answer (`change_versions` of other users, `stack --remote`) return `NOT_AVAILABLE`.
  - Writes → child `asc <verb> --json` (M-D7), spawned from `current_exe()`.
- **`ServeBackend`** calls `RepoHandlers`, `Changes` and `Projects` in-process with the principal from the auth layer. The id mode is always `GIT` at the MCP boundary (SPEC §11.1: git SHAs by default).
- **HTTP mount:** `axum::Router::nest_service("/mcp", StreamableHttpService)` in stateless mode, inside the existing `HttpRoutes` registry (U43a), behind the auth layer and the §16.3 tower limits. The PRM document is served at `/.well-known/oauth-protected-resource/mcp`.

### 6.3 Auth (HTTP)

| Item | Decision |
|---|---|
| Resource server | `asc serve` is an OAuth 2.1 resource server. Requests without a token get 401 with `WWW-Authenticate: Bearer resource_metadata=".../.well-known/oauth-protected-resource/mcp", scope="asc:read"`. |
| Authorization server | Forgejo's OAuth2 provider (fork unit M26: RFC 8707 `resource`, an `aud` claim in access tokens, RFC 9207 `iss`). asc checks `aud` = its canonical `/mcp` URL through the K07 authz answer. Tokens are never passed through to other APIs. |
| Client registration | Pre-registered public client `asc-mcp` (PKCE, loopback redirects) in v1. CIMD in the Forgejo fork is v1.x (M27). DCR is not built (deprecated in 2026-07-28). |
| Forgejo PAT | Accepted as the bearer for headless agents and CI. Its Forgejo scopes map to `asc:read` (`read:repository`) and `asc:write` (`write:repository`). It has no audience, which is acceptable because Forgejo issues it for Forgejo's own resource set. |
| Scopes | `asc:read`, `asc:write`. Insufficient scope gets 403 `insufficient_scope`, so clients step up. |
| stdio | No OAuth. The CLI credentials are used (SPEC §15.1). |

---

## 7. Configuration

| Key | Default | Meaning |
|---|---|---|
| `asc.mcp.write` | `false` | `false` \| `local` \| `remote`. User and system config only; repo and worktree config is ignored. `remote` adds `submit`, `sync` and the verbs that upload (§2.3) |
| `asc.mcp.toolsets` | `core` | comma list: `core,history,stack,projects,search,write`; `all` |
| `asc.mcp.tools` | – | an explicit list; intersected with the toolsets and the write gate |
| `asc.mcp.confirm` | `destructive` | `always` \| `destructive` \| `never`. User and system config only |
| `asc.mcp.maxTokens` | `6000` | per-call budget (hard cap 25,000) |
| `asc.mcp.textMode` | `git` | `git` \| `json` |
| `asc-serve.toml [mcp]` | `enabled = false` until M13 (then `true`; `true` is refused while no auth layer exists), `write = false, allowed_origins = [], hmac_keys = [], max_tokens = 6000, max_inflight_per_principal = 8` (per instance) | the server endpoint |
| Forgejo `app.ini [mcp]` | `ENABLED = true, WRITE = false` | the forge endpoint |

CLI: `asc mcp [--read-only | --write[=local|remote]] [--toolsets <list>] [--tools <list>] [--worktree <path>]` and `asc mcp install [--scope project|user] [--no-forgejo]`. Flags override config only to narrow: they may lower `--write`, remove toolsets and set `--read-only`, never widen past the user or system ceiling (M-D4), even when a committed `.mcp.json` supplies them. `asc mcp install --scope user` therefore writes the user config keys instead of widening flags; a project-scope install writes only narrowing flags. `--read-only` overrides everything.

---

## 8. Proposed PLAN units

These follow the PLAN conventions: small units, disjoint owned files, and one acceptance command each. The stdio acceptance tests are inspector-style: an rmcp client (dev-dependency) spawns `asc mcp` and runs `server/discover`, `tools/list` and tool calls against a fixture repo. An optional CI job also runs the MCP inspector CLI, pinned to an exact version (M16), against the same fixture.

**Amendments to existing units** (recorded hunks, as in PLAN §C):

| Existing unit and file | Hunk | Made by |
|---|---|---|
| U41 `crates/asc-rpc/build.rs`, `lib.rs` | write the descriptor set; `pub const DESCRIPTOR_SET` | M01a |
| U41 `proto/asc/v1/{repo,workspace,projects}.proto`; C17 `changes.proto`; U73 and C31 `workspace.proto` hunks | `(asc.v1.json)` options on `bytes` fields | M01b |
| owner of `crates/asc/src/cli/json.rs` (U21a/U73) | print through `asc_rpc::json` | M01c |
| C07 `crates/asc/src/stack/json.rs`; C15 `op log` output | reconcile to the profile schemas: snake_case, `tz` on `Signature`, `op log` wrapped as `{ops: [...]}` | M02 |
| U21a `asc status` text output | call the `asc-engine::render` porcelain v2 renderer | M04a |
| U43a `crates/asc-serve/src/lib.rs` | one `HttpRoutes` registration line | M12a |
| `asc-serve.toml [mcp]` default `enabled` | `false` until M13, then `true` | M13 |
| U48a `Commit` handler | write a `ChangeMeta` (SPEC §8.7 gap) | M18 |

**Rust units**

| ID | Title | Crate | Depends | Owns | Accept |
|---|---|---|---|---|---|
| M01a | `options.proto`, the JSON profile codec, descriptor export | asc-rpc | U41 | `proto/asc/v1/options.proto`, `crates/asc-rpc/src/json.rs` | `cargo test -p asc-rpc json:: && buf lint proto` |
| M01b | Annotate the existing protos with `(asc.v1.json)` | asc-rpc | M01a, C17, C31, U73 | `crates/asc-rpc/tests/json_annotations.rs` (the hunks of the amendments table) | `cargo test -p asc-rpc --test json_annotations && buf lint proto` (every `bytes` field has an option) |
| M01c | `--json` prints through the profile | asc | M01b, U21a, U73 | `crates/asc/tests/json_profile.rs` | `cargo test -p asc --test json_profile` (golden `--json` of status, diff and log; `asc api` stays proto3 JSON) |
| M02 | `cli.proto` (`OpResult` with `status`/`candidates`, `OpLogResponse`, `*Args`, `WorktreeAddResult`, `AscHandles`) and reconciliation of the C07 shapes | asc-rpc, asc | M01c, C07, C15 | `proto/asc/v1/cli.proto`, `crates/asc/tests/mcp_json_shapes.rs` | `cargo test -p asc --test mcp_json_shapes` |
| M03 | asc-mcp skeleton, `tools.toml`, schema generator, catalogue, `tools/list` snapshot and token budget | asc-mcp | M01b, M02, C17, C31, G09, C30a, C30b | `crates/asc-mcp/{Cargo.toml,build.rs,tools.toml}`, `src/{lib.rs,catalogue.rs,schema.rs}`, `schema/asc_handles.schema.json`, `tests/snapshots/**` | `cargo test -p asc-mcp -- catalogue:: schema:: && cargo deny check` |
| M04a | Text renderers moved into the engine: porcelain v2 status, `--oneline` log, blame, `path:line:text` grep | asc-engine | U15, U21a | `crates/asc-engine/src/render/{mod.rs,status.rs,log.rs,blame.rs,grep.rs}` | `cargo test -p asc-engine render::` |
| M04 | Shaping: stream folding, budget (bytes/3), truncation, cursors | asc-mcp | M03, M04a | `src/{shape.rs,cursor.rs}` | `cargo test -p asc-mcp -- shape:: cursor::` (adds the 300-file diff cap, byte-offset cursors, HMAC key ring) |
| M05 | Handler core: discover, tools/call, error mapping (exit 1 and 2 are not errors), gating, concurrency, progress, cancel | asc-mcp | M03, M04 | `src/{handler.rs,backend.rs,config.rs,errors.rs}` | `cargo test -p asc-mcp handler::` (in-memory duplex transport; config-trust tests: repo config and flags cannot widen) |
| M06 | MRTR elicitation and sealed `requestState`, escaped summaries | asc-mcp | M05 | `src/confirm.rs` | `cargo test -p asc-mcp confirm::` (replay with changed arguments is refused; expired state is refused; a client without elicitation gets `NEEDS_CONFIRMATION`; a hostile commit subject is escaped and truncated) |
| M07 | Resources, templates, completion, prompts | asc-mcp | M05 | `src/{resources.rs,prompts.rs}` | `cargo test -p asc-mcp -- resources:: prompts::` |
| M08 | `RpcBackend` over `RpcClient` and the IPC `WorkspaceClient` | asc-mcp | M05, U42, U52, C17, G09 | `src/rpc_backend.rs`, `tests/rpc_backend.rs` | `cargo test -p asc-mcp --test rpc_backend` (in-process tonic server with `EngineBackend`) |
| M09 | `asc mcp` stdio, `LocalBackend` routing (daemon start, `ASC_NO_DAEMON` in-process with per-call state and schema check, server, local-only), path gate, stdout guard | asc | M07, M08, U73, C31, U50 | `crates/asc/src/mcp/{mod.rs,backend.rs,pathgate.rs}`, `crates/asc/src/cli/mcp.rs`, `crates/asc/tests/mcp_stdio.rs` | `ASC_NO_DAEMON=1 cargo test -p asc --test mcp_stdio` (local-only fixture: discover, list, status, diff, log, show, read_file ranged, ls_tree paging, grep, blame, smartlog; snapshot results; `..`, symlink, junction and unlisted `worktree` are refused; a hook printing to stdout does not break the stream) |
| M09b | `asc mcp install` | asc | M09 | `crates/asc/src/mcp/install.rs`, `crates/asc/tests/mcp_install.rs` | `cargo test -p asc --test mcp_install` (project scope writes only narrowing flags; user scope writes user config; never adds the HTTP endpoint next to `asc mcp`) |
| M10 | Local write tools through child verbs (`current_exe`, null stdin, graceful cancel, lock recovery); confirmations; `undo` by op id | asc | M09, M06, C10, C11, C12, C15, C24, C25, U54w | `crates/asc/src/mcp/write.rs`, `crates/asc/tests/mcp_write.rs` | `cargo test -p asc --test mcp_write` (create → modify → restack → undo round trip; `undo` of a non-latest op gives `STALE_OP`; a cancelled child leaves no `index.lock`; `submit` against a test server needs confirmation; tools absent when `asc.mcp.write=false`) |
| M11 | Local `subscriptions/listen` over `Workspace.Watch` | asc | M09, C49 | `crates/asc/src/mcp/listen.rs`, `crates/asc/tests/mcp_listen.rs` | `cargo test -p asc --test mcp_listen` |
| M12a | `asc serve /mcp` transport: stateless Streamable HTTP, Origin and header checks, body caps, mount off by default | asc-serve | M05, U43c | `crates/asc-serve/src/mcp/{mod.rs,limits.rs}`, `crates/asc-serve/tests/mcp_http.rs` | `cargo test -p asc-serve --test mcp_http` (stub backend: POST JSON and SSE replies, GET/DELETE 405, bad Origin 403, absent Origin accepted, header mismatch -32020; `enabled = true` is refused while no auth layer exists) |
| M12b | `ServeBackend` and rate classes | asc-serve | M12a, U47, U69, C19, G09 | `crates/asc-serve/src/mcp/backend.rs`, `crates/asc-serve/tests/mcp_backend.rs` | `cargo test -p asc-serve --test mcp_backend` (read tools through the in-process handlers; each call charged to its SPEC §16.3 class; 8 in-flight per principal and instance; an unreadable repo returns the same `NOT_FOUND` as a missing one) |
| M13 | Server auth: PRM, 401/403, bearer validation (PAT and OAuth `aud` via K07), scope-filtered `tools/list`; flips `enabled` to true by default | asc-serve | M12b, U44, K07 | `crates/asc-serve/src/mcp/auth.rs`, `crates/asc-serve/tests/mcp_auth.rs` | `cargo test -p asc-serve --test mcp_auth` |
| M18 | Server `Commit` writes a `ChangeMeta` (SPEC §8.7 gap) | asc-serve | U48a, C18 | `crates/asc-serve/tests/commit_change_meta.rs` (plus the U48a hunk) | `cargo test -p asc-serve --test commit_change_meta` |
| M14 | Server write tools: `commit` (empty or `refs/for/*` only), `submit`, `rebase`, `restack` | asc-serve | M13, M06, M18, U48a, U68, C22, C24 | `crates/asc-serve/src/mcp/write.rs`, `crates/asc-serve/tests/mcp_write.rs` | `cargo test -p asc-serve --test mcp_write` (a `commit` with `update_ref = refs/heads/master` is refused; `refs/for/*` runs the §8.3 pipeline) |
| M15 | `search` tool (listed when `search.v1` is advertised) | asc-mcp | M08, code-intel `Search` RPC (to be designed) | `tools.toml` row, `src/search.rs` | `cargo test -p asc-mcp search::` |
| M16 | Budgets and conformance: RSS, latency, `tools/list` tokens; inspector CLI job | asc, bench | M10, M12b | `crates/asc/tests/mcp_budgets.rs`, `crates/asc/tests/inspector/{package.json,package-lock.json}`, `.forgejo/workflows/mcp-inspector.yml` | `cargo test -p asc --test mcp_budgets -- --ignored` (idle RSS ≤ 24 MiB; ≤ 64 MiB with 8 concurrent 25k-token diffs on the HUGE generator repo). The workflow has `on:` filters (`push` to `master` and `pull_request`, paths `crates/asc-mcp/**`, `crates/asc/src/mcp/**`, `crates/asc-serve/src/mcp/**`, `proto/**`), checks out with `asc ci checkout` (SPEC §12.7), and installs the inspector with `npm ci` from the committed lockfile (no floating `npx`) |
| M17 | Generated tool reference `docs/mcp.md` (drift-checked, including the `AscHandles` schema hash) | asc-mcp | M03 | `crates/asc-mcp/src/bin/asc-mcp-docs.rs`, `docs/mcp.md` | `cargo run -p asc-mcp --bin asc-mcp-docs -- --check` |

Critical path: M01a → M01b → M01c → M02 → M03 → M04 → M05 → M08 → M09 (the local read surface, works in local-only mode) → M10. M04a runs in parallel with M01a–M03 and gates M04. The server path is M05 → M12a → M12b → M13 → M14 (M18 before M14). M06, M07, M09b and M17 run in parallel after their dependencies.

**Forgejo units** (Go, `D:\dev\forgejo`, branch `asc/v16`, one commit per unit; commands run from `D:\dev\forgejo`)

SDK decision: the official `modelcontextprotocol/go-sdk`, pinned to its first release that implements 2026-07-28. If no such release exists when M20 starts, M20 implements the thin stateless POST transport itself and uses only the SDK's types and JSON-RPC code. No other Go MCP library is evaluated.

| ID | Title | Owns | Depends | Accept |
|---|---|---|---|---|
| M20 | MCP framework: `/api/mcp` stateless Streamable HTTP, `server/discover`, API auth reuse, Origin check, `[mcp]` settings, read-only gate (write methods also need `lockdown` available, registered by M28), PRM document | `routers/api/mcp/{server.go,transport.go,auth.go}`, `modules/setting/mcp.go` | K01, K07 | `go test ./routers/api/mcp/... -run TestMcpTransport` |
| M21 | `pr_read` (with `AscHandles` and stack panel); vendors `asc_handles.schema.json` | `routers/api/mcp/pr_read.go`, `routers/api/mcp/schema/asc_handles.schema.json` | M20, M03, C34, C36 | `go test ./routers/api/mcp/... -run TestMcpPrRead` |
| M22 | `pr_write` (stack-aware merge, 409 mapping, auto-merge) and `review_write` (confirmation on approve and dismiss) | `routers/api/mcp/{pr_write.go,review_write.go}` | M21, M28, C38, C39 | `go test ./routers/api/mcp/... -run 'TestMcpPrWrite\|TestMcpReviewWrite'` |
| M23 | `issue_read`, `issue_write` (confirmation on all methods but `label`) | `routers/api/mcp/issue.go` | M20 | `go test ./routers/api/mcp/... -run TestMcpIssue` |
| M24 | `actions_read` (ranged and tailed logs with a 64 KiB cap), `actions_write` | `routers/api/mcp/actions.go` | M20, K14, C40 | `go test ./routers/api/mcp/... -run TestMcpActions` |
| M25 | `inbox_read`, `inbox_write` | `routers/api/mcp/inbox.go` | M20, C41b | `go test ./routers/api/mcp/... -run TestMcpInbox` |
| M26 | OAuth: RFC 8707 `resource`, `aud` in access tokens, RFC 9207 `iss`, the pre-registered `asc-mcp` client, `aud` and scopes in the K07 authz answer | `services/oauth2_provider/resource_indicator.go`, `routers/web/auth/oauth_asc_mcp.go`, `routers/private/asc.go` hunk | K07 | `go test ./services/oauth2_provider/... -run TestResourceIndicator` |
| M27 | (v1.x) CIMD client registration | `services/oauth2_provider/cimd.go` | M26 | `go test ./services/oauth2_provider/... -run TestCimd` |
| M28 | `lockdown` mode (v1): hide content by authors without write access in `pr_read`, `issue_read`, `actions_read`; registers `lockdown.Available()` so write methods can be enabled | `routers/api/mcp/lockdown.go` | M21, M23, M24 | `go test ./routers/api/mcp/... -run TestLockdown` |

---

## 9. Edits needed in SPEC.md and PLAN.md (not made here)

- **SPEC §0/§1.1:** add the MCP decision (M-D1–M-D10) to the design page and the traceability table.
- **SPEC §7:**
  - add `options.proto` and `cli.proto`;
  - add the `/mcp` and `/.well-known/oauth-protected-resource/mcp` routes;
  - say that `asc api` keeps standard proto3 JSON (this section's promise stays), and that CLI `--json` prints the versioned asc JSON profile `json.profile.v1` (§3.2), which differs in field casing, id and path rendering, enums and 64-bit integers; SPEC §11.2 and §20.7 currently disagree with each other on casing, and this settles it.
- **SPEC §8.1:** `asc-serve.toml [mcp]`. **SPEC §8.7:** a server `Commit` must write a `ChangeMeta` for the changeset it creates, so changes created through MCP get change ids (gap; unit M18).
- **SPEC §20.7:**
  - under branchless mode (the checklist default, switchable per repo or per user) `asc create` implies `--no-branch`; with branchless off it names a branch from `asc.stack.branchTemplate` as today. Name the config key in the same edit;
  - the `Signature` example gains `tz`;
  - `asc op log --json` prints `{"ops": [...]}`;
  - `asc undo` and `asc redo` take `--op <id>` and refuse (`STALE_OP`) when it is not the latest;
  - the exit-code line says that under `--json` exit codes 1 and 2 print `status: "paused"` / `"ambiguous"` with `conflicts` / `candidates`.
- **SPEC §11.2:** add the `asc mcp` and `asc mcp install` asc-only commands.
- **SPEC §13.3:** M20–M27 rows. **SPEC §15.1:** MCP scopes, the audience check and the PAT rule. **SPEC §16.3:** MCP calls are charged to their backing RPC's class, plus 8 in-flight calls per principal. **SPEC §16.4:** `asc mcp` RSS budgets.
- **SPEC (code intelligence, pending):** a `Search` RPC with `search.v1` in `Version.features`. M15 depends on it.
- **PLAN:** the unit index rows M01a–M01c, M02–M18 (including M04a, M09b, M12a, M12b) and M20–M28, the amendments table above, the `rmcp` pin in `[workspace.dependencies]`, and `crates/asc-mcp` in the workspace layout and dependency-direction lines.

---

## Review notes

Rejected or changed review items:

- Item 1, user-scope `.mcp.json` exception: not implemented as a flag exception. The server cannot tell which `.mcp.json` launched it, so a widening flag could come from a committed file. `asc mcp install --scope user` writes the user config keys instead (§7), which gives the same result.
- Item 8, "mark `undo` destructive or split it": both done. `undo` is marked `d`, and `redo` is a separate tool.
- Item 22, Go SDK: decided, not left open (§8, Forgejo units): the official SDK, pinned, with a thin own transport as the only fallback.
