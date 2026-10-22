# ASC-built IDE plugins for asc

Status: design for the checklist decision "ASC-built IDE plugins" (2026-10-22). Folded into SPEC.md v1.8 (§24) and PLAN.md (§P) on 2026-10-22; where this file and SPEC §24 differ, SPEC wins (SPEC §24.10 lists the rulings). §9 listed the edits those files needed. Reviewed once (see "Review notes" at the end).

User decision of 2026-10-22: IDE support is wanted, built by ASC as plugins that match GitKraken and Graphite in one place and replace the built-in git. "No IDE support" in older text now means only "asc does not rely on an IDE's built-in git and has no full `.git` projection" (`scion-checklist.md:204`).

Inputs: the VS Code API (stable `vscode.d.ts`, the proposal files, the built-in git extension), GitLens and GitKraken docs and community complaints, the Graphite VS Code extension and web review, Sapling ISL and its VS Code extension, jjk, GG, jj-idea, hg4idea, Dracon and the IntelliJ Platform SDK (research of 2026-10-22). asc sources: `scion-checklist.md`, SPEC §7, §9, §11, §12A, §15, §20, `stacks.md` (§3 verbs, §5 plugin API), `branchless.md`, `mcp.md` and `codeintel.md`.

Fixed rules this design obeys:
- The plugins talk only to the asc daemon IPC, the asc server RPC (`asc.v1`) and the Forgejo API. One generated TypeScript client package (from proto) serves both IDE plugins and the Forgejo fork UI.
- They never parse human CLI output. Every value they read comes from proto messages.
- They replace the built-in git SCM. There is no full `.git` projection.
- They follow the asc efficiency budgets: lazy activation, no whole-repo scans, virtualised lists, paging.
- The default branch is `master`, never `main`. Branchless is the default.
- TypeScript tooling is Bun, Turborepo, and Hono where a server is needed. JetBrains uses Kotlin and Gradle.

`stacks.md` still states "there is no IDE extension". The 2026-10-22 decision supersedes that line (§9).

---

## 0. Decisions

| # | Decision | Why |
|---|---|---|
| ID1 | **Two layers in each IDE.** The IDE's native API is used wherever it has a primitive: SCM status, gutters, badges, blame, hover, CodeLens, diff, merge, comments, quick pick, status bar and simple trees. Everything with a graph, drag and drop, a stack, review navigation or a chart lives in **one shared React UI, `asc-ui`**. The same `asc-ui` runs in a VS Code webview, a JetBrains JCEF tool window and as islands in the Forgejo fork. | This is the Sapling ISL model: a native SCM view plus a rich smartlog webview. One implementation of the hard UI for three hosts. Native primitives are faster, use less memory and work with the keyboard. |
| ID2 | **One transport: the `asc api --stdio` bridge.** Each IDE host spawns one long-lived `asc api --stdio` child. It carries length-prefixed `asc.v1.BridgeFrame` protobuf frames. The bridge routes each call to the daemon IPC (`Workspace`), the asc server (`Repo`, `Changes`, `Projects`, `Cloud`, `CodeIntel`) or the in-process engine. The routing is asc's own: the same `LocalBackend` routing as `asc mcp` (M09). | Discovery (`/.well-known/asc`), credentials (`git credential fill`), tiers, `CLIENT_TOO_OLD`, no-daemon mode and Windows named pipes already exist in Rust. Each plugin reimplementing them would duplicate the hardest code twice. The bridge relays the same messages unchanged, so the rule "talk only to IPC and RPC" holds. |
| ID3 | **Reads are RPCs. Writes are asc verbs run through `Bridge.Run`**, which returns the typed `OpResult` of `cli.proto` (M02). The bridge validates the argv against a per-verb flag schema (§3.5.1), spawns `asc <verb> --json …`, and decodes the output with `asc_rpc::json`, the single codec. | The same rule as mcp.md M-D7. Hooks, the op log, the ref chokepoint and locks are reused exactly. TypeScript and Kotlin only ever see protobuf, never JSON text and never human output. |
| ID4 | **Webviews hold no tokens and no process access.** `asc-ui` calls a `Platform` interface. Its RPCs go through a host relay that forwards only the entries of the checked-in `allowlist.json` (§3.5.1): named RPCs, named verbs with a flag schema, and (method, path template) pairs for Forgejo. Merge, review submit and other destructive writes need a native confirmation dialog that the host builds from the parsed request. The host adds the Forgejo token; the webview never sees it. | This is ISL's security model. A compromised webview cannot merge, delete, call admin endpoints, inject git options or read credentials. |
| ID5 | **No proposed VS Code APIs.** The commit graph is a webview, not `scmHistoryProvider`. Timeline is our own history view. The action button becomes `acceptInputCommand` plus title buttons. | Marketplace and Open VSX builds cannot enable proposals (§3.9). GitLens ships the same way. |
| ID6 | **Replace built-in git, and be inert elsewhere.** The plugins claim only asc workspaces: a root `.git` gitfile whose gitdir contains `asc-workspace` (SPEC §9.4). VS Code: the workspace setting `git.enabled: false`, offered once and checked into ASC. JetBrains: `vcs.xml` maps the roots to `asc`. A non-asc folder costs one `stat` and nothing else. | The built-in git would otherwise drive asc through the shim and duplicate every view. A plain git repo in the same window keeps working. |
| ID7 | **Branchless first.** The status bar shows the change id, the stack position and the cloud state, not a branch. The primary action is "Create change" (`asc create --no-branch`), not "Commit to branch". Branches appear only as labels. The trunk is `master`. | Checklist decision "branchless by default", plus BD1–BD3. |
| ID8 | **Never gate a local feature behind an account.** Status, commit, stacks, graph, blame, history, undo, conflicts and affected all work signed out and offline. Only Forgejo-backed views ask for sign-in. | The top complaint against GitLens 19 and GitKraken. |
| ID9 | **Event-driven only.** The `Watch` stream (paths, HEAD, ref, stage, op, switch, `KIND_STACK`, `KIND_CLOUD`) is the only refresh trigger. There is no FileSystemWatcher on `**` and no timer polling of asc state. The exceptions are the no-daemon fallback (§5) and the Forgejo views, which have no event stream and use ETag-conditioned polls with the caps of §3.4 (inbox, Actions runs). | The cost of a refresh is then O(changed paths), with no repo walks. |
| ID10 | **Optimistic operation queue.** Each `asc-ui` mutation has a pure preview applier over the `Stacks` model. The UI shows the predicted state at once, queues verbs in order and keeps the preview until `Watch` confirms. `--dry-run` predicts conflicts before a drag is dropped. | ISL's lifecycle (preview, queued, running, optimistic, done) is why it feels instant over a CLI. |
| ID11 | **JetBrains: native VCS providers, but no `VcsLogProvider`.** `readAllHashes` loads the whole history, which breaks the no-whole-repo-scan rule. The graph is `asc-ui` in JCEF, as jj-idea builds its own graph. | Native providers give the Commit window, gutters, Annotate, Show History, the merge tool and Rollback for free. |
| ID12 | **AI stays out of the plugins.** Agents get asc through MCP. The plugin registers `asc mcp` and Forgejo `/api/mcp` with the IDE and injects no token: the IDE's own MCP OAuth flow (§3.8) obtains one. | stacks D12 and mcp.md M-D1. No upsell surface. |
| ID13 | **Bun for all TypeScript** (install, build, test, scripts), Turborepo for orchestration, Hono only for the `asc-ui` dev host and the mock Forgejo. JetBrains is Gradle (Kotlin DSL), wrapped as Turbo tasks. | One toolchain. Hono is small and runs on Bun. |
| ID14 | **Never cause a hydration storm.** On a virtual workspace the plugins do not walk, read or index working files themselves; every content read goes through `Workspace.ReadFile` (store, no hydration). They write exclusions for generated folders and watch the per-process hydration counters (§3.11). | IDE indexers, search and watchers are the main source of accidental hydration (SPEC hydration policy, line 1284). |
| ID15 | **Every cache and queue has a cap.** Ops queue, comment threads, op log, `Run` children, avatar and content caches all have a stated limit and debounce (§3.12). | Long sessions must not grow the extension host or JVM. |

---

## 1. Repo layout

The TypeScript and Kotlin code lives in this repo next to the Rust crates, so `proto/` is the single source.

```
package.json               Bun workspaces ["packages/*", "plugins/*"], packageManager bun@<pinned>,
                           devDeps: turbo, typescript, oxlint (ASC anti-slop rules), @biomejs/biome (format), @bufbuild/buf
turbo.json                 tasks: codegen → build → typecheck, lint, test, test:e2e, package
tsconfig.base.json         strict, noUncheckedIndexedAccess, exactOptionalPropertyTypes, verbatimModuleSyntax
proto/buf.gen.ts.yaml      protoc-gen-es v2 (target ts) → packages/asc-client-ts/src/gen/ (generated, gitignored, Turbo-cached)
proto/asc/v1/bridge.proto   BridgeFrame, Bridge service (Hello, Run, Token, StoreToken, Events)            (P01a)
packages/asc-client-ts/     @asc/asc-client: gen/, transport/{bridge-node,relay,host,grpc-web}.ts, transport/allowlist.json, forgejo/, ids.ts
packages/asc-ui/            @asc/asc-ui: React 19, Jotai, CSS modules, @vscode/codicons, TanStack Virtual;
                           src/{platform.ts,theme/,state/,ops/,views/<view>/}; builds dist/ (ESM, code-split per view)
packages/asc-ui-devhost/    @asc/asc-ui-devhost: Hono on Bun; serves asc-ui in a browser against a fixture bridge,
                           plus a mock Forgejo (inbox, pulls, reviews, Actions) typed from the fork's swagger
packages/asc-fixtures/      @asc/asc-fixtures: deterministic fixture repos built with the asc CLI (§6.4)
plugins/vscode/            asc.asc extension: src/, test/, package.json (manifest); Bun.build bundle
plugins/jetbrains/         IntelliJ Platform plugin: build.gradle.kts, settings.gradle.kts, src/main/kotlin/asc/asc/**,
                           package.json whose scripts call ./gradlew (so Turbo orders and caches it)
```

| Package | Contents | Size budget |
|---|---|---|
| `@asc/asc-client` | protobuf-es v2 messages and Connect-ES v2 service descriptors for `repo`, `workspace`, `changes`, `projects`, `cloud`, `codeintel`, `cli`, `bridge`. Transports: `bridgeNode` (spawns and frames `asc api --stdio`), `relay`/`host` (webview ↔ host, allowlisted), `grpcWeb` (browser → Forgejo proxy, P04). The Forgejo REST client (`openapi-typescript` types generated from the fork's `swagger.v1.json`, plus `openapi-fetch`) with an injectable `fetch`. Id helpers (hex, k–z change ids). | ≤ 120 KB min, tree-shaken per consumer |
| `@asc/asc-ui` | Views (§2), the `Platform` interface, theme tokens (`--asc-*`), state, the ops queue | initial ≤ 300 KB gzip; each lazy view ≤ 150 KB gzip |
| `asc.asc` (VS Code) | Extension host bundle, one CJS file, `vscode` external | ≤ 350 KB min (excluding `asc-ui` assets) |
| JetBrains plugin | Kotlin providers, protobuf-java messages generated from `proto/` by the Gradle protobuf plugin, the `asc-ui` dist as jar resources | jar ≤ 4 MB |

**Build.** `bun run codegen` runs `buf generate` (Turbo inputs: `proto/**`). The extension host is bundled with `Bun.build({ target: "node", format: "cjs", external: ["vscode"] })`. CJS output is marked experimental in Bun, so the `@vscode/test-electron` suite always runs against the bundle. `scripts/bundle.ts` is the one file to swap to esbuild if that guard ever fails. `asc-ui` is built with `Bun.build` (HTML entry, code splitting, CSS). JetBrains `processResources` copies `packages/asc-ui/dist`, and the Turbo task `@asc/jetbrains#build` depends on `@asc/asc-ui#build`. Rust is not built by Turbo: tests read `ASC_BIN` (default `target/release/asc`) through `passThroughEnv`.

**Bridge protocol (`bridge.proto`, P01a–P01c).**

```proto
message BridgeFrame { uint32 id = 1;
  oneof f { Call call = 2; bytes reply = 3; End end = 4; bool cancel = 5; } }
message Call { string method = 1;      // "asc.v1.Workspace/Status", "asc.v1.Changes/Stack", "asc.v1.Bridge/Run", …
  bytes request = 2; bool server_stream = 3; string root = 4; /* workspace root; selects ws_id, repo, remote */ }
message End { int32 code = 1; string message = 2; bytes error_detail = 3; /* asc.v1.ErrorDetail */ }
service Bridge {
  rpc Hello(HelloRequest) returns (HelloResponse);   // asc version, features (below), roots → {ws_id, repo, remote kind, forge URL,
                                                     //  branchless, daemon|server|forge state}
  rpc Run(RunRequest) returns (stream RunEvent);     // verb + args validated against the verb's flag schema; --json added; no stdin;
                                                     //  events: stderr line, OpResult, exit
  rpc Token(TokenRequest) returns (TokenResponse);   // forge host → bearer; host-only, never relayed to a webview; never prompts
  rpc StoreToken(StoreTokenRequest) returns (StoreTokenResponse);  // git credential approve; fails NO_CREDENTIAL_HELPER (§5)
  rpc Events(EventsRequest) returns (stream BridgeEvent);          // connectivity changes, daemon restarts
}
message RunRequest { string verb = 1; repeated string args = 2; /* flags and positionals, split by the host; no shell */
  string root = 3; /* no stdin field: the child's stdin is the null device */ }

// HelloResponse.features is the union of (a) bridge flags: "plugin.v1" (this protocol), "json-verbs.v1" (every verb of
// allowlist.json has a --json form, P45), "reorder-todo.v1" (P03), "split-hunks.v1" (P46); and (b) Repo.Version.features
// copied from the asc server when one is configured ("search.v1" is added there when CodeIntel is enabled, codeintel.md:207).
// HelloResponse.asc_version is for display only. Gating uses features, never version comparison (§3.1).
```

Frames are `u32 little-endian length ‖ BridgeFrame`, at most 16 MiB each, so streams stay chunked. Cancellation drops the server call or kills the `Run` child (M-D7). The host restarts the bridge with backoff (0.2 s, 1 s, 5 s) and resubscribes to `Watch` with its last token.

---

## 2. Feature matrix

Seen in: **GK** GitKraken Desktop, **GL** GitLens, **GT** Graphite (CLI, VS Code, web), **ISL** Sapling ISL, **VG** VS Code built-in git, **JB** JetBrains Git. "Run x" means `Bridge.Run` of the asc verb `x` (typed `OpResult`). VS Code v1 and JetBrains v1 mark what ships in the first release of each plugin. A dash means v1.x.

### 2.1 Working copy and commit

| Feature | Seen in | VS Code surface | JetBrains surface | asc API | VSC v1 | JB v1 |
|---|---|---|---|---|---|---|
| Changes view: conflicts, staged, changes, untracked | all | SCM provider, four resource groups | `ChangeProvider` → Commit tool window | `Workspace.Status`, `Watch` | Y | Y |
| Stage and unstage file, hunk, selected lines | GK GT VG JB | `scm/resourceState/context`, `scm/change/title` (gutter peek), editor command | Commit window partial selection → patch | `Workspace.Stage` (`add`/`remove`/`patch`) | Y | Y |
| Discard | all | resource menu with a modal confirmation | `RollbackEnvironment` | Run `restore --staged --worktree` (`--json` form: P45) | Y | Y |
| Quick-diff gutter against `HEAD` | VG JB | `quickDiffProvider` → `asc:` URIs | `DiffProvider`/`ContentRevision` (line status tracker) | `Workspace.ReadFile` (P02) | Y | Y |
| Explorer and tab badges | VG JB | `FileDecorationProvider`, targeted invalidation | `FileStatus` from `ChangeProvider` | `Status` | Y | Y |
| Create change, amend, reword | all | input box + `acceptInputCommand` (Ctrl+Enter = create), title buttons | `CheckinEnvironment` + `AmendCommitAware`; executors "Create change", "Create and submit" | Run `create --no-branch`, `modify`, `reword` | Y | Y |
| Absorb with preview | GT ISL | SCM title button → preview quick pick → apply | Commit window action | Run `absorb -n`, then `absorb -f` | Y | Y |
| Hook output (husky, commitlint) | GK | Output channel; a failure shows stderr in a notification | VCS console | `RunEvent.stderr` | Y | Y |
| Uncommit, unamend, abandon | ISL | stack menu and commands (P26b) | `asc-ui` Stack tab context menu (P15) | Run `uncommit`, `unamend`, `abandon` (C51) | Y | Y |
| Multi-root and nested worktrees | GK GL | one `SourceControl` per asc root | one VCS root mapping per root | `Bridge.Hello` per root | Y | Y |
| Stash: push (with untracked), pop, apply, drop, list | GK VG JB | SCM `...` menu and a "Stashes" group in the Worktrees tree | – (v1.x; JetBrains Shelf is not wired to `refs/stash`) | Run `stash push\|pop\|apply\|drop\|list` (`--json`: P45) | Y | – |
| Tags: list, create, delete | GK GL | – (v1.x; needs `Repo.ListRefs`, §7 API additions) | – | Run `tag` | – | – |
| Fetch, pull, push (git-remote mode only; hidden in server mode, where `sync` and `submit` apply) | GK VG JB | SCM `...` menu, status-bar sync | VCS menu actions | Run `fetch`, `pull`, `push` (`--json`: P45) | Y | Y |
| Add to `.ascignore` (from an untracked or ignored file) | GK | resource menu → appends the pattern to the nearest `.ascignore` through a `WorkspaceEdit` | `IgnoredFileContentProvider` for `.ascignore` + "Add to .ascignore" action | file edit; `Status` then refreshes via `Watch` | Y | Y |
| Signing | GK | no settings UI (out of scope, §2.6); a "signed" badge in commit details | same | `commit.gpgSign` is honoured by `create`/`modify`; the badge is `%G?` from `Log` | – | – |

### 2.2 Stacks and branchless (Graphite, ISL)

| Feature | Seen in | VS Code surface | JetBrains surface | asc API | VSC v1 | JB v1 |
|---|---|---|---|---|---|---|
| Smartlog: drafts relative to `master` with PR state, checks, needs-restack, divergent, frozen, worktree, version | GT ISL | `asc-ui` smartlog in a webview view in the SCM container | `asc-ui` tab "Stack" | `Workspace.Stacks`, `Changes.Stack`/`ResolveChanges`, `Watch KIND_STACK` | Y | Y |
| Uncommitted node with per-file checkboxes → "create change" or "modify ‹change›" | GT | smartlog | smartlog | `Stage`, Run `create`/`modify --into` | Y | Y |
| Drag a change onto a new parent (moves descendants) | GT ISL GK GG | `asc-ui` drag and drop with predicted-conflict badge | same | Run `move --onto -n`, then `move` | Y | Y |
| Stack editor: reorder, fold, drop, reword, committed in one op | ISL GK GL | `asc-ui` stack editor | same | Run `reorder --todo` (P03) | Y | Y |
| Split a change: interactive dialog that picks hunks and lines per piece | ISL GT GK | `asc-ui` split dialog | same | `Repo.Diff` of the change, then Run `split --hunks <spec>` (P46; the interactive `--by-hunk` is never run by a plugin) | Y | Y |
| Restack, sync, continue, abort | GT ISL | smartlog buttons, commands, `scm/title` buttons (P47a), status-bar "needs restack" | actions | Run `restack`, `sync`, `continue`, `abort` | Y | Y |
| Navigate up, down, top, bottom | GT | commands with keybindings (P47a); at a fork, a quick pick of `OpResult.candidates` | actions | Run `up`/`down`/`top`/`bottom` (exit 2 → candidates; C09) | Y | Y |
| Submit stack, update PRs as new versions | GT | smartlog, command, `scm/title` | smartlog | Run `submit [-s]` | Y | Y |
| Submit options: draft or publish, reviewers, auto-merge | GT | `asc-ui` submit dialog; reviewer list from Forgejo `GET …/assignees` | same | Run `submit [--draft\|--publish] [--reviewer <u>…] [--auto-merge]` (modal confirmation) | Y | Y |
| PR title and description | GT | submit dialog and review panel; the PR text defaults to the change message | same | Forgejo `PATCH …/pulls/{n}` (allowlisted body: `title`, `body`, `draft`) | Y | Y |
| Merge stack (queue) | GT | smartlog, review panel | same | `Changes.RequestMerge` (relayed, modal confirmation) or Run `stack merge` (SPEC §20.7) | Y | Y |
| Get a stack (frozen), optionally into a new worktree | GT ISL | command with PR or change quick pick | action | Run `stack get [--worktree]` | Y | – |
| Hide and unhide drafts; other machines' drafts | ISL | smartlog menu, "Show cloud drafts" toggle | same | Run `hide`/`unhide` (B05); `Cloud.GetWorkspace`, or Run `sl --cloud --user <me>` (B16); `Watch KIND_CLOUD` | Y | Y |
| Cloud upload state | – | status bar item ("synced", "N pending", "offline"); owner P47b | status bar widget | `Bridge.Events`, `Watch KIND_CLOUD` | Y | Y |
| Undo and redo, op log | GK ISL | status-bar undo/redo, "Op log" tree | `asc-ui` tab | Run `undo`/`redo`, `op log` | Y | Y |
| Change versions and interdiff | ISL GT | `asc-ui` details | same | `Changes.Versions`, `Changes.InterDiff` | Y | Y |
| Optimistic queue (ID10) | ISL | `asc-ui` ops | same | Run + `Watch` | Y | Y |
| Branch labels (opt-in) | GK GL | labels on graph rows; "Name this change" | same | `Stacks.branches`, Run `create <branch>` | Y | Y |
| Share a snapshot link | – | command, copies `asc:<id>` and the web link (P47a) | action (v1.x) | Run `share` (C32) | Y | – |

### 2.3 History and insight (GitLens, GitKraken)

| Feature | Seen in | VS Code surface | JetBrains surface | asc API | VSC v1 | JB v1 |
|---|---|---|---|---|---|---|
| Inline blame on the current line, status-bar blame | GL VG | `TextEditorDecorationType` `after`, status bar | Annotate gutter, status-bar widget | `Workspace.Blame` with `buffer` (C31) | Y | Y |
| Blame hover: commit, change, PR, autolinks, "blame previous revision", open changes | GL | `HoverProvider` scoped to the active file | annotation tooltip | `BlameResponse.commits[].change`, `reviews[]`, `RepoConfig` | Y | Y |
| Whole-file blame and heatmap | GL JB | toggle: gutter decorations by age | native Annotate (age colours) | `Blame` | Y | Y |
| CodeLens: recent change, authors | GL | `CodeLensProvider`, lazy `resolveCodeLens` | – | `Blame` (cached) | Y | – |
| File history, renames followed | all | tree view "File History" for the active editor | `VcsHistoryProvider` (Show History) | `Workspace.Log` `paths`, `follow`, `with_stats`, paged | Y | Y |
| Line history (selection) | GL JB | command → the same tree in line mode | `getVcsBlockHistoryProvider` (Show History for Selection) | `LineHistory` (truncated + resume) | Y | Y |
| Revision navigation (previous and next version of the file) | GL | `editor/title` buttons over `asc:` URIs | native Compare with previous | `Log` + `ReadFile` | Y | Y |
| Visual file history (swim lanes, size by churn) | GL | `asc-ui` editor panel | `asc-ui` tab | `Log` `with_stats` (`diffstat.v1`) | Y | – |
| Commit graph: lanes, minimap, scroll markers, column toggles, search, keyboard navigation, WIP rows per worktree | GK GL | `asc-ui` editor panel; compact graph in the sidebar view | `asc-ui` tab | `Workspace.CommitGraph` / `Repo.CommitGraph` (C29, C31); minimap data in §3.5 | Y | Y |
| Commit details: message, files, diff, change, PR | all | `asc-ui` details; files open in `vscode.changes` (multi-diff) | `asc-ui` details; native diff | `ReadChangeset`, `ResolveChanges`, `Diff` | Y | Y |
| Compare two revisions, changes or stacks | GK GL JB | `asc-ui` compare + `vscode.changes` | `asc-ui` + native diff | `Compare` (C30b), `Diff` | Y | Y |
| Search commits by message, author, file, code change | GK GL JB | quick pick + graph filter | `asc-ui` graph filter | `Log` `grep`/`author`/`paths`, pickaxe (C30c) | Y | Y |
| Worktrees: list, create, open, remove, dirty, ahead/behind | GK GL | tree view "Worktrees" | `asc-ui` tab | `Workspace.Worktrees`, Run `worktree add/remove` (`--json`: P45) | Y | Y |
| Cherry-pick and revert from the graph | GK GL | graph context menu (asc-ui in P17, host wiring in P47c) | same | Run `cherry-pick`/`revert` (`--json`: P45) | Y | Y |

### 2.4 Conflicts

| Feature | Seen in | VS Code surface | JetBrains surface | asc API | VSC v1 | JB v1 |
|---|---|---|---|---|---|---|
| 3-way merge editor | GK JB VG | `_open.mergeEditor` with `asc:` inputs (feature-detected); fallback: the marked file plus VS Code's built-in conflict-marker CodeLens | `MergeProvider` → native merge tool (non-conflicting changes in one click) | `Workspace.Conflicts`, `ReadFile`, `Stage` | Y | Y |
| Conflict list, in-progress banner, continue and abort | GK | SCM "Conflicts" group, title buttons, status bar | Commit window | `Status.in_progress`, Run `continue`/`abort` | Y | Y |
| Predicted conflicts before a move or restack | – | `asc-ui` badges during drag | same | `--dry-run` `OpResult.conflicts`, `Changes.Mergeability` | Y | Y |

### 2.5 Review, inbox, CI, projects, search, agents

| Feature | Seen in | VS Code surface | JetBrains surface | asc API | VSC v1 | JB v1 |
|---|---|---|---|---|---|---|
| Launchpad inbox: needs your review, returned, ready, blocked, waiting, drafts, queued; pin and snooze | GK GL GT | tree view "Inbox" + status-bar count | `asc-ui` tab | Forgejo `GET /api/v1/user/asc/inbox` (C41b) | Y | Y |
| Stack-aware review: navigator, per-change diff, version picker, interdiff, "since my last review", overlap and mergeability badges | GT | `asc-ui` review panel; files open in native diff editors | `asc-ui` review tab + native diff | `Changes.Stack`/`Versions`/`InterDiff`/`Overlaps`/`Mergeability`, `Diff`; Forgejo pulls | Y | Y |
| Inline review comments in diff editors | GT | Comments API (`comments.createCommentController`) on `asc:` diff URIs | comment list in `asc-ui` (inline: v1.x, §2.6) | Forgejo reviews and comments API | Y | – |
| Approve, request changes, merge (stack guard 409) | GT | review panel, commands | review tab | Forgejo reviews and merge (review submit and merge: modal confirmation); `Changes.RequestMerge` | Y | Y |
| Check out a PR's stack | GT GK | command from the inbox or review | action | Run `stack get` (C26) | Y | Y |
| Actions: runs and jobs per change, log tail | GT (checks) | tree view "Actions"; logs as read-only `asc-actions:` documents | `asc-ui` tab | Forgejo Actions REST (P05) | Y | Y |
| Affected projects (uncommitted, change, stack) | – | tree view "Affected" | `asc-ui` tab | `Workspace.Affected` (`uncommitted`), `Projects.Affected` | Y | Y |
| Project graph | – | `asc-ui` panel | `asc-ui` tab | `Projects.ProjectGraph` | Y | – |
| Code search: exact and regex at any revision, change or stack | – | quick pick (streamed) + `asc-ui` results panel | `asc-ui` tab | `CodeIntel.Search` (I16, I18), fallback `Repo.Grep` | Y | Y |
| Symbol and semantic search | – | `asc-ui` search modes; `Go to asc symbol` command | `asc-ui` tab | `CodeIntel.Symbols`, `CodeIntel.Semantic`, `IndexStatus` | Y (when `Hello.features` has `search.v1`) | Y (same) |
| MCP for agents | GL | `lm.registerMcpServerDefinitionProvider` (`asc mcp` stdio + Forgejo `/api/mcp`, no token injected, §3.8); Cursor: `vscode.cursor.mcp.registerServer` | action "Configure agents" → Run `mcp install` | M09, M20 | Y | Y |

**Out of scope:** AI commit messages and AI compose (ID12), cloud workspaces shared across a team (GitKraken), GitHub, GitLab and Jira integrations, LFS (No LFS decision), git-flow, submodule editing (submodules are read-only compositions, SPEC §10.6), paid tiers, a signing settings UI (signing is `commit.gpgSign`/`gpg.*` in `asc config`; the plugins only show the badge), command palette, repo tabs, profiles and work items (GitKraken desktop features, stacks.md fixed rules).

### 2.6 JetBrains v1 limits and the reason for each

| Missing in JetBrains v1 | Why | Target |
|---|---|---|
| Inline review comments in diff editors | No stable public comments API for diff editors is verified for 2026.1 and 2026.2; the review tab in `asc-ui` carries threads | v1.x, after the API survey in spike P51 |
| CodeLens-style authors and recent change | The platform's code vision is internal API in `261`; Annotate and the status-bar widget cover blame | v1.x |
| Stash, tags, project graph, visual file history, share, `stack get --worktree` | Not worth a native surface yet; stash and share are Run verbs reachable from the Stack tab later | v1.x |
| Everything a `VcsLogProvider` feeds (§4.4 lists them) | ID11: `readAllHashes` loads the whole history | not planned; the `asc-ui` graph replaces it |
| Split mode and remote development | The providers need the backend; the UI needs the frontend | after v1 (§4) |

---

## 3. VS Code plugin

Extension id `asc.asc`, published to the VS Marketplace and Open VSX (Cursor, VSCodium). `engines.vscode: ^1.101.0`, the first release with a stable MCP server definition provider. No `enabledApiProposals`.

### 3.1 Activation and lifecycle

| Step | What | Cost |
|---|---|---|
| Activation events | `workspaceContains:.git` (a root `stat`, not a glob), `onFileSystem:asc`, `onUri`, `onWebviewPanel:asc.graph`, `onWebviewPanel:asc.review`, `onWebviewPanel:asc.history`. Views, commands and the auth provider activate implicitly. Never `*`. | – |
| `activate()` | For each folder: read `.git` (one small file), `stat` `<gitdir>/asc-workspace`. If no folder is an asc workspace, register nothing and return. | ≤ 5 ms |
| asc folder found | Register the contributions synchronously. Spawn the bridge lazily (fire-and-forget); `Hello` returns roots, features and state. Then `Status`, then `Watch`. | `activate()` ≤ 30 ms; first SCM status ≤ 300 ms on ASC (warm daemon) |
| Built-in git | If `git.enabled` is true for an asc folder, show one notification: "Use asc instead of built-in git for this workspace" → writes `"git.enabled": false` at workspace scope. ASC checks the setting into `.vscode/settings.json`. | once |
| `asc` binary | Setting `asc.path`, else `PATH`, else `$ASC_HOME/bin`. The minimum is defined by features, not a version number: `Hello.features` must contain `plugin.v1` and `json-verbs.v1` (the first asc release that ships P01a–P01c, P02, P03, P45 and P46). Otherwise the plugin shows "Update asc" (with `asc_version` for display) and disables mutating commands; read-only views keep working when `plugin.v1` is present. `CLIENT_TOO_OLD` from the server shows the same notice. | – |
| Dispose | Close the `Watch` streams and end the bridge (stdin EOF). Webview panels are disposed when closed. | – |

Source layout (`plugins/vscode/src`), each module ≤ 400 lines:

| Module | Responsibility |
|---|---|
| `extension.ts` | activation, detection, registration |
| `bridge.ts` | one `bridgeNode` per window; restart; `Events` → state |
| `repo.ts` | per root: the `Status` snapshot, `Watch` subscription, event coalescing (50 ms), the `Stacks` snapshot |
| `scm/` | `SourceControl`, groups, input box, commands, status bar |
| `fs/ascfs.ts` | read-only `FileSystemProvider` for `asc:` |
| `decorations.ts` | `FileDecorationProvider` |
| `blame/` | inline decoration, hover, CodeLens, heatmap |
| `views/` | trees: Worktrees, Inbox, Actions, Affected, Op log, File History |
| `webview/` | panel and view hosts for `asc-ui`, the relay and its allowlist |
| `conflicts.ts`, `review/`, `auth.ts`, `mcp.ts` | §3.6–§3.8, §4 |

### 3.2 SCM provider, `asc:` documents and decorations

- `scm.createSourceControl("asc", "asc", root)` per asc root. Groups: `conflicts`, `staged`, `changes`, `untracked`, hidden when empty. Each `SourceControlResourceState` carries the porcelain `xy` as `contextValue` and opens `vscode.diff(asc:HEAD, file)`, or `(asc:stage, file)` for staged entries.
- Input box placeholder: "Message (Ctrl+Enter to create a change on ‹change›)". `acceptInputCommand` = `asc.create`. `scm/title`: Create, Amend, Absorb, Undo (P26a); Restack, Sync, Submit stack (P47a). `count` = the number of changed files.
- **`asc:` URIs:** `asc:/<path>?root=<ws>&rev=<sha>`, `…&stage=<0-3>` or `…&file=<FileId>`. They are served by `Workspace.ReadFile` (P02), which works offline from the local store and hydrates through the tiers. Immutable content (rev or file id) is cached in an LRU by id (32 MiB). Stage content is invalidated by `KIND_STAGE`.
- **Quick diff:** `quickDiffProvider.provideOriginalResource(uri)` returns `asc:` at `HEAD`. "Diff against stack parent" opens `vscode.diff(asc:parent, file)`, since a second quick-diff provider would be a proposed API.
- **Stage selected ranges or a hunk:** compute the hunks of `asc:HEAD` against the buffer in the host (the `diff` package, file-local), build a `git apply --cached` patch, then call `Workspace.Stage{patch}`.
- **Decorations:** one `FileDecorationProvider`. `onDidChangeFileDecorations` fires only with the URIs from the last `Status(paths)` delta. Colours use the `gitDecoration.*` theme colours so themes keep working.
- **Status bar:** `$(git-commit) kxyzq… · 2/5 · ↓3` (change, stack position, behind `master`), a cloud indicator (P47b), and "needs restack" as a warning item. Clicking it opens the smartlog. `↓3` is the number of `master` (trunk) commits that are not in the stack's base. It comes from the `Stacks` snapshot (`WsStacksResponse.stacks[].behind_trunk`, API addition A1), not from `WsStatusResponse.behind`, which compares the HEAD ref with its upstream and is empty in branchless mode. It is hidden at 0, and clicking it offers "Restack onto `master`".
- **Refresh:** `KIND_PATHS`/`KIND_STAGE` → `Status(paths)` for those paths only. `KIND_HEAD`/`KIND_REF`/`KIND_SWITCH_DONE`/`KIND_RESYNC` → full `Status`. At most one `Status` call per 100 ms per root. The resource list is capped at 10,000 entries, with a "N more, open in terminal" node (the built-in git `statusLimit` default).

### 3.3 Inline blame, hover and CodeLens

| Piece | Implementation | Budget |
|---|---|---|
| Current-line blame | `createTextEditorDecorationType({ after: { color: ThemeColor("asc.blameForeground"), margin: "0 0 0 3em" } })`, set on selection change, debounced 150 ms | ≤ 1 request per 150 ms per editor |
| Data | `Workspace.Blame{rev: "" (working copy), buffer}` for dirty documents (throttled to 500 ms after the last edit), else at `HEAD`. One response covers the whole file, so cursor moves are free. | O(file), served from the `blame.v1` memo |
| Cache | LRU of 100 entries per root, keyed by `(path, HEAD, document version)`. Invalidated by `KIND_HEAD` and saves. | – |
| Hover | Commit subject and message, author and time, change id and version, PR number and state (`reviews[]`), autolinked issue refs (`RepoConfig`), actions: Open changes, Blame previous revision, Open PR, Copy change id | – |
| Whole-file blame and heatmap | Toggle command; `before` decorations per hunk; the heatmap is a gutter border coloured by age quantile | visible ranges only |
| CodeLens | `provideCodeLenses` returns unresolved lenses at top-level symbols (from the document symbol provider). `resolveCodeLens` reads the cached blame. Off for files over 5,000 lines. | no RPC in `provide` |
| Skips | binary files, files over 2 MiB, `asc:` and untitled documents, files outside asc roots | – |

Inline blame is on by default; the setting `asc.blame.inline` turns it off. Built-in git ships it off, but GitLens users expect it on.

### 3.4 Tree views

All trees are `TreeDataProvider`s in the `asc` activity-bar container, except File History, which sits in Explorer. They page with a "Load more" item and fetch only when visible (`onDidChangeVisibility`).

| View | Source | Refresh |
|---|---|---|
| Worktrees | `Workspace.Worktrees` | `Watch` on any root; actions: open in new window, add (`--ephemeral`), remove, lock |
| Inbox | Forgejo inbox | on focus, and every 60 s only while visible (`If-None-Match`; a 304 costs almost nothing). The status-bar count has its own poll, §3.7 |
| Actions | Forgejo runs for the current stack's PRs | every 5 s only while a run is in progress and the view is visible |
| Affected | `Workspace.Affected{uncommitted}` and per change | `KIND_PATHS` (debounced 1 s) |
| Op log | Run `op log -n 50` | `KIND_OP_FINISHED` with a 500 ms trailing debounce, only while the view is visible (otherwise the view is marked stale and refreshes when shown); at most one `op log` child at a time and one per 2 s; "Load more" up to 500 entries (§3.12) |
| File History | `Workspace.Log{paths, follow, with_stats, limit: 50}` for the active editor | active editor change, `KIND_HEAD` |

### 3.5 `asc-ui` webviews: smartlog, graph, review, history

- **Hosts:** one `WebviewViewProvider` (`asc.smartlog`) in the SCM container, and `createWebviewPanel` editor tabs for the graph, review, visual history, project graph and search. Each panel restores through a `WebviewPanelSerializer` (`onWebviewPanel:*`). `retainContextWhenHidden` is false; `asc-ui` keeps its scroll and selection in `vscode.setState`.
- **Security:** the CSP is `default-src 'none'; script-src ${cspSource}; style-src ${cspSource}; font-src ${cspSource}; img-src ${cspSource} data:; connect-src 'none'`, and `localResourceRoots` = the `asc-ui` dist. `font-src` serves the bundled codicons `woff2` from the dist. The webview makes no network request. **Avatars** are not loaded from the forge or from third-party hosts (Forgejo's `avatar_url` can point at Gravatar or another external service); the webview calls `Platform.avatar(url)` and the host fetches it: https only, no cookies and no token, public addresses only (loopback and private ranges are refused, every redirect is re-checked, at most 3 redirects), content type png, jpeg, webp or gif (never SVG), at most 256 KB, 5 s timeout, LRU of 200 entries and 2 MiB, returned as a `data:` URI. JetBrains applies the same rule in Kotlin.
- **Relay:** the webview's `asc-client` uses the `host` transport (`postMessage` of `{id, method, bytes}`). The host forwards a message only when it matches an entry of `allowlist.json` (§3.5.1). "All read RPCs" is not a rule: every RPC, verb and Forgejo operation `asc-ui` uses is classified there, and a unit test fails when a proto RPC or a used swagger operation is unclassified. Three request kinds: (1) asc RPCs by full name; (2) `Bridge.Run` with a verb entry whose flag schema the bridge enforces (the host runs the same check so an invalid call fails early, but the bridge is the authority); (3) Forgejo REST as `{method, path, body}`, matched against (method, path template) pairs with a body schema and a repo scope. Anything else is dropped and logged. The host never shows confirmation text supplied by the webview; it composes the text from the parsed request.
- **Theme:** `asc-ui` uses its own `--asc-*` tokens. The VS Code platform maps them from `--vscode-*` variables and the `vscode-light`/`vscode-dark`/`vscode-high-contrast` body classes.
- **Graph rendering:** rows come from `CommitGraph` pages of 200, with server-computed `column` and `edges`, so the client does no layout. DOM rows are virtualised (TanStack Virtual, overscan 20). One `<canvas>` draws the lanes for the visible window. At most 5,000 rows stay in memory (a ring keyed by page token). A `Watch` event refetches only the first page and splices rows by commit id (GitLens's ledger-diff idea, without the whole ledger). The minimap uses the header of the first `CommitGraphResponse` page (the total row count and `ref_markers[]` with row indexes), not `LogResponse.count`, because the graph and the log are different streams. If the server omits them (API addition A2), the minimap shows only the loaded pages. Keyboard: arrow keys, parent and child, next fork or ref, `/` to search.
- **Smartlog interactions:** single click selects (files of the change, no checkout); double click checks out (Run `switch`); drag reparents; buttons are disabled with a tooltip that says why ("Uncommitted changes: create or stash them first", "Frozen", "Landed").

#### 3.5.1 The relay allowlist (`allowlist.json`) and the bridge argv rules

`packages/asc-client-ts/src/transport/allowlist.json` is the one checked-in source. `scripts/gen-allowlist.ts` (P10) validates it against the proto descriptors and the fork's `swagger.v1.json` (it fails on an unclassified RPC, an unknown verb flag or an unknown path), and P48 generates `Allowlist.kt` from it. The Rust bridge embeds its own per-verb schema (`argv_schema.rs`, P01b) and a test checks that both agree.

```json
{
  "version": 1,
  "rpc": {
    "read": [
      "asc.v1.Workspace/Status", "asc.v1.Workspace/Diff", "asc.v1.Workspace/Log", "asc.v1.Workspace/Blame",
      "asc.v1.Workspace/Affected", "asc.v1.Workspace/Watch", "asc.v1.Workspace/Stacks", "asc.v1.Workspace/Worktrees",
      "asc.v1.Workspace/Conflicts", "asc.v1.Workspace/CommitGraph", "asc.v1.Workspace/ReadFile",
      "asc.v1.Repo/Version", "asc.v1.Repo/Log", "asc.v1.Repo/Diff", "asc.v1.Repo/Blame", "asc.v1.Repo/ReadChangeset",
      "asc.v1.Repo/ReadFile", "asc.v1.Repo/Grep", "asc.v1.Repo/CommitGraph", "asc.v1.Repo/LineHistory",
      "asc.v1.Repo/Compare", "asc.v1.Repo/RepoConfig",
      "asc.v1.Changes/ResolveChanges", "asc.v1.Changes/Stack", "asc.v1.Changes/Versions", "asc.v1.Changes/InterDiff",
      "asc.v1.Changes/Mergeability", "asc.v1.Changes/Overlaps",
      "asc.v1.Projects/ProjectGraph", "asc.v1.Projects/Affected",
      "asc.v1.Cloud/GetWorkspace",
      "asc.v1.CodeIntel/Search", "asc.v1.CodeIntel/Symbols", "asc.v1.CodeIntel/Semantic", "asc.v1.CodeIntel/IndexStatus"
    ],
    "write": [
      { "name": "asc.v1.Workspace/Stage", "confirm": "none", "limits": { "patchBytes": 1048576, "paths": 10000 } },
      { "name": "asc.v1.Changes/RequestMerge", "confirm": "modal" }
    ],
    "never": [
      "asc.v1.Workspace/Ping", "asc.v1.Workspace/Mount", "asc.v1.Workspace/Unmount", "asc.v1.Workspace/Switch",
      "asc.v1.Workspace/Dehydrate", "asc.v1.Workspace/Prefetch", "asc.v1.Workspace/Journal",
      "asc.v1.Workspace/ClearJournal", "asc.v1.Workspace/Stats", "asc.v1.Workspace/Shutdown",
      "asc.v1.Repo/GetObjects", "asc.v1.Changes/RestackChanges", "asc.v1.Changes/SetReviewState",
      "asc.v1.Bridge/Hello", "asc.v1.Bridge/Token", "asc.v1.Bridge/StoreToken", "asc.v1.Bridge/Events"
    ]
  },
  "run": {
    "reject": ["-c", "-C", "--exec", "--exec-path", "--git-dir", "--work-tree", "--config-env", "--namespace",
               "--super-prefix", "--upload-pack", "--receive-pack", "--force", "--force-with-lease"],
    "aliases": "never", "stdin": "none", "endOfOptions": true, "maxArgs": 64, "maxArgBytes": 65536,
    "types": { "rev": "^[^-\\s\\x00-\\x1f][^\\x00-\\x1f]{0,255}$", "path": "inside root after canonicalisation",
               "tmpfile": "absolute, inside $ASC_PLUGIN_TMP, written by the host", "user": "^[A-Za-z0-9._-]{1,40}$" },
    "verbs": {
      "create":      { "flags": { "--no-branch": "bool", "-m": "str:65536", "-a": "bool" }, "pos": "path*" },
      "modify":      { "flags": { "--into": "rev", "-m": "str:65536", "-a": "bool" }, "pos": "path*" },
      "reword":      { "flags": { "-m": "str:65536" }, "pos": "rev?" },
      "absorb":      { "flags": { "-n": "bool", "-f": "bool" } },
      "move":        { "flags": { "--onto": "rev", "--source": "rev", "-n": "bool" } },
      "reorder":     { "flags": { "--todo": "tmpfile" } },
      "split":       { "flags": { "--hunks": "tmpfile" }, "pos": "rev?" },
      "squash":      { "flags": { "-m": "str:65536" }, "pos": "rev*" },
      "fold":        { "flags": { "-m": "str:65536" }, "pos": "rev*" },
      "restack":     { "pos": "rev?" },
      "sync":        {},
      "continue":    {},
      "abort":       {},
      "up":          { "pos": "int:100?" },
      "down":        { "pos": "int:100?" },
      "top":         {},
      "bottom":      {},
      "submit":      { "flags": { "-s": "bool", "--draft": "bool", "--publish": "bool", "--reviewer": "user*",
                                  "--update-only": "bool", "--auto-merge": "bool" }, "pos": "rev?", "confirm": "modal" },
      "stack get":   { "flags": { "--worktree": "bool" }, "pos": "rev" },
      "stack merge": { "flags": { "--now": "bool" }, "pos": "rev?", "confirm": "modal" },
      "stack delete":{ "pos": "rev?", "confirm": "modal" },
      "stack rename":{ "pos": "str:80" },
      "stack freeze":{ "pos": "rev?" },
      "stack unfreeze": { "pos": "rev?" },
      "hide":        { "pos": "rev*", "confirm": "modal" },
      "unhide":      { "pos": "rev*" },
      "abandon":     { "pos": "rev*", "confirm": "modal" },
      "uncommit":    {},
      "unamend":     {},
      "undo":        {},
      "redo":        {},
      "op log":      { "flags": { "-n": "int:500" } },
      "sl":          { "flags": { "--cloud": "bool", "--user": "user" } },
      "share":       { "pos": "rev?" },
      "switch":      { "flags": { "--detach": "bool" }, "pos": "rev" },
      "restore":     { "flags": { "--staged": "bool", "--worktree": "bool", "--source": "rev" }, "pos": "path+", "confirm": "modal" },
      "cherry-pick": { "pos": "rev+" },
      "revert":      { "pos": "rev+" },
      "worktree add":    { "flags": { "--ephemeral": "bool" }, "pos": "path rev?" },
      "worktree remove": { "pos": "path", "confirm": "modal" },
      "stash push":  { "flags": { "-u": "bool", "-m": "str:1024" }, "pos": "path*" },
      "stash pop":   {}, "stash apply": {}, "stash list": {},
      "stash drop":  { "confirm": "modal" },
      "fetch":       { "flags": { "--prune": "bool" } },
      "pull":        { "flags": { "--rebase": "bool", "--ff-only": "bool" } },
      "push":        { "flags": { "-u": "bool" }, "confirm": "modal" }
    }
  },
  "forgejo": {
    "base": "/api/v1", "origin": "the forge URL of an opened asc root", "repoScope": "{o}/{r} must be the forge repo of an opened asc root",
    "pairs": [
      { "m": "GET",  "p": "/user" },
      { "m": "GET",  "p": "/user/asc/inbox" },
      { "m": "PUT",  "p": "/user/asc/inbox/{id}", "body": ["pinned", "snoozed_until"] },
      { "m": "GET",  "p": "/repos/{o}/{r}/assignees" },
      { "m": "GET",  "p": "/repos/{o}/{r}/pulls/{n}" },
      { "m": "GET",  "p": "/repos/{o}/{r}/pulls/{n}/files" },
      { "m": "GET",  "p": "/repos/{o}/{r}/pulls/{n}/commits" },
      { "m": "GET",  "p": "/repos/{o}/{r}/pulls/{n}/reviews" },
      { "m": "GET",  "p": "/repos/{o}/{r}/pulls/{n}/reviews/{id}/comments" },
      { "m": "POST", "p": "/repos/{o}/{r}/pulls/{n}/reviews", "body": ["body", "commit_id", "comments"], "note": "pending review only: `event` must be absent" },
      { "m": "POST", "p": "/repos/{o}/{r}/pulls/{n}/reviews/{id}", "body": ["body", "event"], "confirm": "modal", "note": "review submit" },
      { "m": "PATCH","p": "/repos/{o}/{r}/pulls/{n}", "body": ["title", "body", "draft"] },
      { "m": "POST", "p": "/repos/{o}/{r}/pulls/{n}/merge", "body": ["Do", "merge_when_checks_succeed"], "confirm": "modal" },
      { "m": "GET",  "p": "/repos/{o}/{r}/actions/runs" },
      { "m": "GET",  "p": "/repos/{o}/{r}/actions/runs/{id}" },
      { "m": "GET",  "p": "/repos/{o}/{r}/actions/runs/{id}/jobs" },
      { "m": "GET",  "p": "/repos/{o}/{r}/actions/jobs/{id}/logs" },
      { "m": "POST", "p": "/repos/{o}/{r}/actions/runs/{id}/rerun" }
    ],
    "never": ["every DELETE", "/admin/**", "/orgs/**", "/user/keys/**", "/user/applications/**", "/users/*/tokens/**",
              "PATCH or DELETE /repos/{o}/{r}", "/repos/{o}/{r}/hooks/**", "/repos/{o}/{r}/collaborators/**",
              "/repos/{o}/{r}/branch_protections/**", "/api/mcp"]
  }
}
```

**Confirmation rule.** An entry with `"confirm": "modal"` makes the host show a native modal dialog (VS Code `showWarningMessage({modal: true})`, JetBrains `Messages`) that names the action, the targets (change ids, PR numbers, paths, counts) and the consequence. The default button is Cancel. The same rule applies when a command (not the webview) triggers the call. A `none` write is still bounded by its `limits`. Forgejo bodies are validated against the swagger schema of the operation, and any field not listed in `body` is rejected.

**Bridge argv rules (`argv_schema.rs`, P01b).** The bridge, not the webview and not the host, is the authority:

1. The verb must be a key of the embedded schema. Aliases (`alias.*`), external commands and prefixes are never resolved: the child runs with `ASC_NO_ALIASES=1` (API addition A5).
2. Each argument is a known flag of that verb, in the form `--flag=value`, or a positional of the declared type. Unknown flags are rejected, as are all flags in `run.reject` (this includes `-c`, `-C`, `--exec`, `--exec-path`) even if a verb would otherwise accept them.
3. The bridge builds the child argv itself: `asc <verb> --json <flags…> --end-of-options <positionals…>`. Revs and paths always come after `--end-of-options`, so a value that starts with `-` can never be read as an option.
4. There is no stdin from the webview or the host: the child's stdin is the null device. Messages go in `-m` (at most 64 KiB); a split or reorder plan goes in a `tmpfile` that the host wrote into `$ASC_PLUGIN_TMP` (a per-session 0700 directory) and that the bridge opens itself.
5. The child's environment is rebuilt: a fixed set (`PATH`, `HOME`, `USERPROFILE`, `SystemRoot`, `ASC_*` set by the bridge), `GIT_TERMINAL_PROMPT=0`, `GIT_EDITOR=true`, `GCM_INTERACTIVE=never`, no inherited `GIT_*`.
6. Limits: 64 arguments, 64 KiB per argument, at most 4 `Run` children per bridge (others queue), stderr kept to 1 MiB per child.

### 3.6 Conflicts

1. `Status` reports conflicted paths and `in_progress`. The SCM "Conflicts" group and a status-bar banner ("Restack paused: 2 conflicts · Continue · Abort") appear.
2. Opening a conflict calls `Workspace.Conflicts` for `{base, ours, theirs, marked}`. If `commands.getCommands()` includes `_open.mergeEditor`, it is called with `asc:` inputs (`file=<id>`), as the built-in git does (inputs swapped during a restack). `_open.mergeEditor` is an internal, unversioned command, so it is used only when present.
3. Otherwise the working file already holds the diff3 markers (`marked`). VS Code's built-in `merge-conflict` extension, which does not depend on git, adds its Accept current, incoming or both CodeLens.
4. Saving a file with no markers left offers "Mark resolved" → `Workspace.Stage{add}`. "Continue" runs `asc continue`.

### 3.7 Review, inbox, Actions, affected, search

- **Review:** opening a PR (from the inbox, smartlog or a URI) opens the `asc-ui` review panel: the stack navigator, change list, version picker, interdiff and approve/merge. Files open in native diff editors over `asc:` URIs, or all at once with `vscode.changes`. A `CommentController` (`asc.review`) shows Forgejo review threads on those diff editors. New comments go into a pending review submitted from the panel. Line mapping uses the PR's `merge_base` and head (stacks D8).
- **Inbox:** the tree (§3.4) plus a status-bar count of "needs your review". Pin and snooze go through the inbox API. The count has its own light poll, independent of the view: `GET /user/asc/inbox?section=needs_review&limit=1` with `If-None-Match`, reading `X-Total-Count` (C41b must return it, API addition A6). It runs once on window focus and after any review action, then every 5 minutes while the window is focused and the user is signed in, and stops on blur or when `asc.inbox.statusBar` is false. The item is dimmed with a tooltip "updated N min ago" when the last success is older than 15 minutes or the forge is offline.
- **Actions:** runs, jobs and steps per change (Actions context `asc_change`, C40). A log opens as a read-only `asc-actions:` document filled by range reads (tail 64 KiB, "load earlier"). "Re-run" calls the Forgejo API.
- **Affected:** the tree lists projects with their reasons. "Run target" is a `TaskProvider` of type `asc`: one `vscode.Task` per affected project and target, `new ShellExecution(cmd, args)` with the argv and cwd from the project's target definition (`AffectedResponse.projects[].targets[<t>].argv`, API addition A4), run in the terminal panel; JetBrains uses the terminal tool window. asc does not run or schedule targets, and the task detail shows the equivalent `asc affected --target <t>` command. A project without a target definition does not offer the action. No task runner is built in.
- **Search:** "asc: Search code" is a quick pick that streams `CodeIntel.Search` (workspace scope over IPC), cancels on each keystroke and stops at 200 hits. "Open in panel" shows `asc-ui` search with modes (exact, regex, symbol, semantic), a scope picker (workspace, revision, change, stack) and the index coverage from `IndexStatus`. `search.v1` is advertised by the asc server in `Repo.Version.features` when CodeIntel is enabled (codeintel.md:207) and reaches the plugin as `Hello.features`. When it is absent (including every local-only workspace, which has no server), exact search falls back to the local engine's `Grep` (`Repo.Grep`) and the other modes are hidden with the reason.

### 3.8 MCP registration

- `contributes.mcpServerDefinitionProviders: [{ id: "asc", label: "asc" }]`. The provider returns `McpStdioServerDefinition("asc", ascPath, ["mcp"], { cwd: root })` per asc root and `McpHttpServerDefinition("forgejo", <forge>/api/mcp)`. **No `resolveMcpServerDefinition` token injection:** the `asc-ide` token carries `write:repository`, which an agent channel must not receive. The IDE's own MCP OAuth flow discovers the authorization server from the protected-resource metadata that M20 serves (`/api/mcp` answers 401 with `WWW-Authenticate: Bearer resource_metadata=…`) and gets its own, narrower token. M20 accepts any Forgejo API token (PAT or OAuth2), so the old design was not a protocol error; it is dropped for least privilege. Open point, owned by spike P50: M26 registers only the `asc-mcp` public client, builds no dynamic client registration and leaves client-ID metadata documents to v1.x (M27), so the IDE client may have nothing to register with. Until P50 passes, the provider declares an `inputs` entry (`promptString`, `password: true`) so the user pastes a Forgejo PAT that VS Code stores securely and sends as the bearer; the extension never reads it.
- It never also registers the asc server's `/mcp` (mcp.md placement rule: no tool appears twice).
- Cursor: when `vscode.lm.registerMcpServerDefinitionProvider` is missing, it calls `vscode.cursor?.mcp?.registerServer` with the same two definitions. Otherwise the command "Configure agents" runs `asc mcp install --scope project`.

### 3.9 Proposed-API gaps and the replacements

| Built-in git uses (proposal) | asc plugin instead |
|---|---|
| `scmHistoryProvider` (Source Control Graph) | `asc-ui` graph and smartlog webviews |
| `scmActionButton` | `acceptInputCommand`, `scm/title` buttons, status bar |
| `scmValidation` | validated on accept; an error notification keeps the message in the input box |
| `timeline` | File History tree and visual history panel |
| `quickDiffProvider` (secondary) | "Diff against stack parent" command |
| `scmMultiDiffEditor` | the public `vscode.changes` command |
| merge editor API | `_open.mergeEditor` when present, else conflict markers (§3.6) |

### 3.10 Performance budgets (enforced by P44a for VS Code, P44b for JetBrains)

| Metric | Budget | Measured by |
|---|---|---|
| `activate()` in an asc workspace | ≤ 30 ms, no awaited I/O except the marker read | test-electron timing |
| Cost in a non-asc workspace | ≤ 5 ms, no bridge spawned | test-electron |
| First SCM status visible | ≤ 300 ms after activation (ASC, warm daemon) | test-electron with the fixture |
| Extension-host heap attributable to `asc.asc` at idle | ≤ 25 MiB | The extension host is shared, so per-extension heap cannot be read in a normal window. P44a starts a test-only window (`--disable-extensions --extensionDevelopmentPath=plugins/vscode`, `--js-flags=--expose-gc`) with a probe that forces GC and reads `v8.getHeapStatistics().used_heap_size`, and compares it with the same window on a non-asc folder (the extension inert). The delta is the attributable heap; median of 5 runs |
| Bridge RSS at idle | ≤ 20 MiB | process stats |
| Webviews at idle | at most one (the smartlog view, only when visible) | test |
| Graph | first 200 rows painted ≤ 150 ms after data; scrolling 100,000 rows ≥ 55 fps; ≤ 5,000 rows in memory | Playwright trace (§6) |
| Event handling | `Watch` events coalesced over 50 ms; ≤ 1 `Status` per 100 ms per root | unit test with a fake clock |
| Bundles | host ≤ 350 KB min; `asc-ui` initial ≤ 300 KB gzip; each lazy view ≤ 150 KB gzip | CI size check |
| Never | `*` activation, `createFileSystemWatcher("**")`, `workspace.findFiles` over the repo, unpaged history, polling without a visible view | oxlint rule + review |

### 3.11 Hydration discipline (SPEC hydration policy, line 1284)

On a virtual workspace, a read of a file that is not local hydrates it. IDE search, indexing and watchers are the usual cause. Directory enumeration and `ReadDirectoryChangesW` watchers never hydrate (SPEC line 1284); reading file bodies does.

| Rule | VS Code | JetBrains |
|---|---|---|
| The plugin itself | Content comes from `Workspace.ReadFile` (store, no hydration). It reads working files only for open editors. | Same (`AscContentRevision`). No `VfsUtil` recursive refresh. |
| Exclusions, written once at workspace scope after a prompt and checked in | `files.watcherExclude`, `search.exclude` and `files.exclude` for `**/node_modules`, `**/dist`, `**/.turbo`, `**/target`, `**/.next`, `**/.asc`; `search.followSymlinks: false`. | `DirectoryIndexExcludePolicy` for the same folders; `.ascignore` through `IgnoredFileContentProvider`. Source roots stay indexed: language servers and IDE indexers are expected to hydrate sources, and `asc clone`/`worktree add` prefetch the `.asc/prefetch` globs for that (SPEC 5a). |
| Find in Files | VS Code's text search reads every file it visits. The default is therefore "asc: Search code" (index-backed, no hydration, §3.7); the plugin never rebinds Ctrl+Shift+F. | `asc` tool window search tab; the IDE's Find in Files stays available. |
| Watching the counters | The host reads `Workspace.Stats` (never relayed to a webview) once a minute while a window is focused and `asc debug hydrations --by-process` semantics apply. If one editor process (`rg`, `Code Helper`, an indexer) caused more than `asc.vfs.hydrationBudget` hydrations in a day, show one notification per 24 hours: "Search is hydrating files. Use asc: Search code, or exclude folders" with the top three folders. | Same, from Kotlin, with a link to the excludes. |
| `asc.vfs.denyHydration` | Not changed by the plugin. The notification names the process image so the user can add it. | Same. Windows Search images are already denied by default. |

P49a and P49b test this on a virtual fixture: after activation, a window reload, a search-for-text and an indexing run, `asc debug hydrations --by-process` shows 0 hydrations from the plugin and none beyond manifests from the watcher.

### 3.12 Limits and debounce

| State | Limit | Behaviour at the limit |
|---|---|---|
| `asc-ui` ops queue | 50 queued ops per root; 1 running; 20 finished ops kept for the status line | New ops are refused with "Wait for the running operations"; a queued op whose preview no longer applies after a `Watch` event is cancelled and shown as such |
| `Run` children | 4 per bridge; stderr 1 MiB per child | Others wait in order; excess stderr is truncated with a marker |
| Op log | `-n 50`, "Load more" to 500; refresh debounced 500 ms, one per 2 s, visible view only | Older entries: "open `asc op log` in a terminal" |
| Review comment threads | 200 threads per PR loaded; pages of 50; comment text 64 KiB; `CommentController` threads of a closed diff editor are disposed; at most 20 PR controllers (LRU) | "N more threads, open in the review panel" |
| `asc:` content cache | 32 MiB LRU by id (immutable content) | Evict oldest |
| Blame cache | 100 entries per root | Evict oldest |
| Avatars | 200 entries and 2 MiB | Evict oldest |
| Status resources | 10,000 entries | "N more" node (§3.2) |
| Graph rows | 5,000 in memory (ring) | Page tokens evicted and refetched |
| Event coalescing | `Watch` 50 ms; `Status` one per 100 ms per root | – |

---

## 4. JetBrains plugin

One IntelliJ Platform plugin for IntelliJ IDEA, Rider, RustRover, PyCharm and WebStorm. Kotlin, IntelliJ Platform Gradle Plugin 2.x, `intellijIdea("2026.1")` (Java 21), `sinceBuild = 261`, no `untilBuild`. Depends on `com.intellij.modules.vcs`. `com.intellij.modules.jcef` is declared as an optional dependency (`<depends optional="true" config-file="asc-jcef.xml">`) with a runtime `JBCefApp.isSupported()` check, because the claim that the module becomes required from 2026.2 is unverified. Spike P54 settles it against 2026.1 and 2026.2 with `verifyPlugin`. Split mode and remote development are not supported in v1; the code is kept in `vcs/` (backend-side) and `ui/` (frontend-side) packages so a later split stays mechanical.

### 4.1 Components

| Class | Platform API | asc call |
|---|---|---|
| `AscBridgeService` (application service) | coroutines, `Disposable` | one `asc api --stdio` per IDE process; protobuf-java messages; `Flow` for streams |
| `AscVcs` | `AbstractVcs` (name `asc`), `<vcs name="asc" vcsClass="asc.asc.vcs.AscVcs"/>` | – |
| `AscRootChecker` | `vcsRootChecker` | `.git` gitfile + `asc-workspace` marker |
| `AscMappingActivity` | `ProjectActivity` | maps asc roots to `asc` in `vcs.xml` and drops `Git` mappings for them; one notification; also suppresses Git4Idea's "unregistered VCS root" notice for the synthetic `.git` (spike P52, §4.4) |
| `AscChangeProvider` | `ChangeProvider` (background, dirty scopes) | `Workspace.Status(paths of the dirty scope)` → `ChangelistBuilder` |
| `AscWatchService` (project service) | `VcsDirtyScopeManager.filesDirty`, message bus topic `ASC_STATE` | `Workspace.Watch` |
| `AscContentRevision`, `AscDiffProvider` | `ContentRevision`, `DiffProvider`, `RevisionSelector` | `Workspace.ReadFile` (P02), `Log` |
| `AscCheckinEnvironment` | `CheckinEnvironment` + `AmendCommitAware`; `vcs.changes.localCommitExecutor` "Create and submit" | `Workspace.Stage` of the selected changes, then Run `create --no-branch` / `modify` / `submit`. Partial-line commits need `arePartialChangelistsSupported` and the partial-changelist tracker, which are not assumed: spike P53 decides; until it passes, v1 commits whole files and stages hunks and lines in the `asc-ui` smartlog |
| `AscRollbackEnvironment` | `RollbackEnvironment` | Run `restore --staged --worktree` |
| `AscAnnotationProvider` | `AnnotationProvider` → `FileAnnotation` (revision, author, date, change, PR in the tooltip) | `Workspace.Blame` (with the unsaved buffer) |
| `AscHistoryProvider` | `VcsHistoryProvider.reportAppendableHistory` (pages of 50); block history | `Workspace.Log{paths, follow}`; `LineHistory` |
| `AscMergeProvider` | `MergeProvider.loadRevisions` → `MergeData`; `conflictResolvedForFile` | `Workspace.Conflicts`, `ReadFile`, `Stage` |
| `AscCheckoutProvider` | `checkoutProvider` (Get from VCS) | Run `clone` (`--json`: P45) |
| `AscToolWindowFactory` | tool window `asc` (JCEF, created on first open) | `asc-ui` tabs: Stack, Graph, Review, Inbox, Actions, Affected, Worktrees, Search, History |
| `AscStatusBarWidgetFactory` | `statusBarWidgetFactory` | change id, stack position, cloud state, "needs restack" |
| `AscActions` | action group in VCS menu and Commit toolbar | up, down, restack, sync, submit, absorb, undo, redo |
| `AscConfigurable` | `projectConfigurable` under Version Control | `asc` path, inline blame, forge account |

Not registered in v1: `logProvider` (ID11), `pushSupport` (asc has no separate push step for drafts; `submit` is an action), `UpdateEnvironment` (sync is an action).

### 4.2 JCEF host for `asc-ui`

- Check `JBCefApp.isSupported()`. Without JCEF, the tool window shows a notice and a link to the same view on the Forgejo fork (§4.3 islands).
- `JBCefBrowser` loads `https://asc-ui.local/index.html`. A `CefRequestHandler`/`CefResourceRequestHandler` serves the bundle from jar resources (the `JCefImageViewer` pattern), so nothing is loaded from the network.
- **Relay:** `JBCefJSQuery` carries base64 bridge frames JS → Kotlin, and `executeJavaScript("window.ascHost.deliver(...)")` carries them back. The allowlist and confirmation rules of §3.5 apply unchanged (same table, generated into Kotlin from `packages/asc-client-ts/src/transport/allowlist.json` by the Gradle task `generateAllowlist`, P48; the argv rules of §3.5.1 are enforced by the bridge).
- **Theme:** Kotlin computes the `--asc-*` tokens from `JBColor`/`UIManager` on open and on `LafManagerListener`, and pushes them as a `theme` message. `JBCefScrollbarsHelper` styles the scrollbars.
- **Lifecycle:** the browser is created on first open and disposed when the tool window has been hidden for 10 minutes. `asc-ui` restores its state through `Platform.getPersistedState` (`PropertiesComponent`).
- **Host actions** (`Platform.openFile`, `openDiff`, `confirm`) map to `FileEditorManager`, `DiffManager` with `AscContentRevision`s, and `Messages`.

### 4.3 Forgejo fork reuse

`asc-ui` is built a second time as a self-contained ESM bundle and vendored into the fork at `public/assets/asc-ui/<version>/` (P07). A Go template partial mounts an island: `<div data-asc-ui="graph" data-repo="owner/repo">`. In the browser, the `Platform` uses the `grpcWeb` transport to a same-origin, read-only proxy `/-/asc/rpc/*` (P04). The proxy checks Forgejo's repo permission, then forwards with the internal token. Writes from the web go through Forgejo's own handlers (merge, restack button). Suggested adopters: the commit graph page (C42) and visual file history; the existing Go templates stay valid.

### 4.4 Degraded features and the spikes that settle them

Without a `VcsLogProvider` (ID11), these platform features are expected to degrade or disappear for `asc` roots. Spike P51 runs each on 2026.1 and records the real behaviour in `plugins/jetbrains/spikes/RESULTS-vcslog.md`; the list below is the starting hypothesis, not a finding.

| Platform feature | Expected effect | Replacement in v1 |
|---|---|---|
| Git tool window "Log" tab, "Show in Log", "Go to hash/branch" | absent for `asc` roots | `asc` tool window Graph tab; hash links open there |
| Compare with Branch / Tag / Revision | absent or file-only | `asc-ui` compare (P18) and `DiffProvider`-based "Compare with previous" |
| Directory and project history | file history works through `VcsHistoryProvider`; directory history works only if `supportsHistoryForDirectories` is true | Graph tab with a path filter (`Log{paths}`) |
| Cherry-pick, revert, reset from the Log context menu | absent | graph context menu in `asc-ui` (P47c is VS Code only; JetBrains uses the same `asc-ui` menu through the relay) |
| Annotate gutter "Show in Log" click | opens nothing | opens the `asc` tool window on that change |
| Branches popup, Update Project | the `Git` popup is absent | actions in `AscActions` |

Other JetBrains risks and their spikes (all start from P36, results recorded under `plugins/jetbrains/spikes/`):

| Spike | Question | Decision rule |
|---|---|---|
| P51 | What degrades without `VcsLogProvider`? Is there a stable comments API for diff editors in 2026.1 and 2026.2? | Fill the table above; the API survey decides whether inline review comments move from v1.x to v1 |
| P52 | With `Git4Idea` enabled, does the synthetic `.git` gitfile make the IDE report an unregistered Git root, offer to add it, or map it to `Git`? | Pick the least invasive fix: `vcs.xml` ignored roots, a `VcsRootChecker` that claims the root, or disabling `Git4Idea` for the project; otherwise document it |
| P53 | Can `AscCheckinEnvironment` support `arePartialChangelistsSupported` (partial-line commit from the Commit window) with a `ContentRevision` backed by `ReadFile` and a patch for `Workspace.Stage`? | If not within the spike, v1 ships whole-file commit plus `asc-ui` line staging, and §2.1 row "Stage ... selected lines" becomes `–` for JetBrains in the Commit window |
| P54 | Is `com.intellij.modules.jcef` a module id in 2026.1 and required in 2026.2? Does `verifyPlugin` pass with it optional? | Use optional if the verifier passes on both; else required from `sinceBuild` that needs it |
| P55 | Does the OAuth loopback survive a moved built-in-server port, Remote Dev, and `state`/PKCE checks? Does Forgejo accept a variable loopback port for `asc-ide` (P06)? | Built-in server if stable; else an own `ServerSocket` on a random loopback port |

---

## 5. Auth, offline and local-only

| Topic | Decision |
|---|---|
| Credential store | asc's first: `ASC_TOKEN`, else `git credential fill` for the forge host (SPEC §15.1). The bridge runs with `GIT_TERMINAL_PROMPT=0`, `GCM_INTERACTIVE=never`, `GIT_ASKPASS` unset and no editor, so `fill` never opens a Git Credential Manager dialog at startup: with no stored credential `Token` returns `NOT_FOUND` and the UI shows "Sign in". `Bridge.StoreToken` first checks that a credential helper is configured (`credential.helper` is set and is not `store`); with none (typical on Linux, WSL and Remote) it fails with `NO_CREDENTIAL_HELPER` and writes nothing. It never uses the plaintext `store` helper. Where a helper exists, the CLI, `asc mcp`, VS Code and JetBrains share one sign-in. |
| Credential fallback | On `NO_CREDENTIAL_HELPER` the host keeps the token in VS Code `SecretStorage` (key `asc.asc.token.<forge host>`) or the JetBrains `PasswordSafe`, and passes it to the bridge as `ASC_TOKEN` at spawn. The bridge is respawned when the token changes, and a host-supplied value wins over an inherited `ASC_TOKEN`. In this mode the CLI and `asc mcp` do not share the sign-in; the UI says so once and suggests `git config credential.helper`. Under Remote or WSL the bridge runs on the workspace side and VS Code proxies `SecretStorage` from the local side. Files and settings never hold tokens. |
| asc server calls | Authenticated inside the bridge. The plugin never sees the asc server token path. |
| Forgejo REST calls | Bearer token from `Bridge.Token`, attached by the host (extension host or Kotlin). Never relayed to a webview. |
| VS Code sign-in | `AuthenticationProvider` `asc-forgejo`. `getSessions` → `Bridge.Token`. `createSession` → Forgejo OAuth2 authorization code with PKCE for the pre-registered public client `asc-ide` (P06). The redirect is `${env.uriScheme}://asc.asc/auth` through `env.asExternalUri` + `onUri`, so VS Code, Cursor and VSCodium all work. The token is then saved with `Bridge.StoreToken`, or in `SecretStorage` when that fails with `NO_CREDENTIAL_HELPER`. A PAT can be pasted instead. |
| JetBrains sign-in | The same OAuth flow with PKCE, a `state` value (128 random bits, single use, valid 5 minutes; a callback is ignored unless `state` and the PKCE verifier both match) and a loopback redirect. The port is not fixed: the IDE built-in server listens on `BuiltInServerManager.getInstance().port` (63342 by default, higher when taken, different under Remote Dev), so the redirect is built from the live port, and P06 must accept any port on `127.0.0.1` for `asc-ide` (RFC 8252 §7.3). If the built-in server is unavailable the plugin opens its own `ServerSocket` on `127.0.0.1`. Spike P55 verifies both. A pasted PAT is the alternative; it is stored in `PasswordSafe` (or `StoreToken`) and never in settings. |
| Scopes | `read:user`, `read:repository`, `write:repository` (submit goes through asc, but merge goes through Forgejo), `read:issue`, `write:issue` (reviews and comments). `asc-ide` is a separate client from `asc-mcp` (M26). |
| Signed out | Every local feature works (ID8). Forgejo views show one "Sign in to Forgejo" button. No nags and no trials. |

**Connectivity states.** `Bridge.Hello` and `Bridge.Events` report three independent states: daemon (up, restarting, none), asc server (online, offline, none = git-remote or local-only mode), forge (online, offline, signed out).

| Feature group | Daemon down | Server offline | Local-only or git remote (Stage 0) | Forge offline or signed out |
|---|---|---|---|---|
| Status, stage, create, amend, discard, quick diff | CLI in-process through the bridge (plain workspaces); virtual workspaces wait for the daemon restart (≤ 10 s) | works | works | works |
| Blame, file history, graph | local engine | local engine within the fetched window; older history shows "available when online" | local engine | works |
| Stacks, restack, absorb, split, move, undo | works | works | works | works |
| Cloud drafts | – | queued; status bar shows "N pending" | not applicable | works |
| Submit, stack get, stack merge, `Changes.*` badges | – | disabled with reason | disabled ("needs an asc server", the exit-128 message) | submit works; PR state hidden |
| Review, inbox, Actions | works | works | works when Forgejo exists | disabled with reason |
| Affected | local graph (`--uncommitted`) | local | local | works |
| Code search | daemon overlay index | daemon degraded local index (I19) | `Repo.Grep`-style local fallback | works |

Disabled controls always say why (ID8). The UI never blocks on the network: cheap RPCs time out at 2 s, expensive ones show progress and can be cancelled. When the daemon is absent (`ASC_NO_DAEMON` or plain mode with no daemon), `Watch` is unavailable. VS Code then uses one `createFileSystemWatcher` per root with the `.ascignore`/`.gitignore` excludes and a 1 s debounce. This is the only file watcher in the design.

---

## 6. Testing

### 6.1 Layers

| Layer | Tool | Scope | When |
|---|---|---|---|
| Bridge (Rust) | `cargo test -p asc --test bridge --test bridge_run --test bridge_auth` | framing, routing, cancel kills `Run` children, no-daemon path; argv injection corpus (`-c`, `-C`, `--exec`, `--exec-path`, aliases, `-x` after `--`, values starting with `-`, stdin attempts) is rejected, and every allowlisted verb has a `--json` form; `Token` never prompts and never leaves stdout unrequested; `StoreToken` fails with `NO_CREDENTIAL_HELPER` and writes nothing when no helper is set | PR |
| `@asc/asc-client` | `bun test` | transports against a real `asc api --stdio` on a fixture; the relay allowlist; Forgejo client against the mock | PR |
| `@asc/asc-ui` logic | `bun test` | preview appliers equal the real result on fixture DAGs (apply verb, re-read `Stacks`, compare); ops queue ordering; graph ring buffer | PR |
| `@asc/asc-ui` UI | Playwright (Chromium) against `asc-ui-devhost` | each view's flows; axe accessibility; screenshots in light, dark and high contrast; keyboard-only navigation; graph performance trace (100,000 rows) | PR |
| VS Code | `@vscode/test-cli` + `@vscode/test-electron` (Mocha in the extension host) on the bundled extension, VS Code stable and the `engines` minimum, Windows and Linux | SCM groups and counts, decorations, blame text, `asc:` content, commands produce the expected `OpResult`, conflicts flow, relay rejects non-allowlisted RPCs, verbs, flags and Forgejo paths and shows the modal for each `confirm` entry, activation budgets, inert in a non-asc folder | PR |
| VS Code packaged smoke | `bun plugins/vscode/scripts/smoke-vsix.ts --editor vscodium` (also `--editor vscode`; Cursor's engine is covered by pinning its VS Code version) | installs the built VSIX headless, opens the `dirty` fixture, asserts activation, the SCM group counts through a probe command, and inertness in a non-asc folder | PR (P35b) |
| VS Code budgets | `bun bench/plugins/collect.ts --target vscode --check` | the budgets of §3.10, with the heap probe in a test-only extension-host window | PR (P44a) |
| JetBrains providers | Gradle `test` with `HeavyPlatformTestCase` on the fixture | `ChangeProvider` output, annotation, history paging, merge data, checkin | PR |
| JetBrains UI | IntelliJ IDE Starter + Driver (`runIdeForUiTests`) | Commit window commit, Annotate, merge dialog, `asc` tool window loads `asc-ui` and shows the stack | nightly |
| JetBrains compatibility | `verifyPlugin` (Plugin Verifier) against IU, RD, RR, PY and WS 2026.1 and 2026.2 | API compatibility | PR |
| Forgejo islands and proxy | Go tests (P04, P07) + Playwright against the docker-compose Forgejo | proxy allowlist and permission checks, island mount | PR (Go), nightly (Playwright) |
| Mock Forgejo contract | the same requests against the mock and the real fork | the mock does not drift from the fork's swagger | nightly |

### 6.2 Fixture repos (`@asc/asc-fixtures`)

Built only with the `asc` CLI (`ASC_BIN`), with fixed `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE`, identity and `asc.branchless=true`. Building a scenario twice must give identical change ids and SHAs (P12 acceptance). A Hono mock Forgejo and an in-process `asc serve --db sqlite::memory: --store memory://` (the K-track pattern) provide the server side.

| Scenario | Contents |
|---|---|
| `clean` | `master` with 200 commits |
| `dirty` | staged, unstaged, untracked, ignored, rename, mode change, binary, a 10,000-line file, a `.ascignore` |
| `stacks` | two stacks of 3 and 5 changes, one needs restack, one divergent, one frozen; one branch label; two hidden drafts |
| `conflict` | a restack paused on two conflicted files (content and modify/delete) |
| `worktrees` | three workspaces, one ephemeral, one nested under `.claude/worktrees/` |
| `history` | 2,000 commits with renames, for blame, line history and the graph |
| `review` | the `stacks` scenario submitted to the in-process server, with mock PRs, reviews, inbox entries and Actions runs |
| `big` (nightly) | `bench/gen` repo with 100,000 files and 100,000 commits, virtual mode, for performance budgets |

---

## 7. Proposed PLAN units (P01a–P56)

PLAN conventions apply: small units, disjoint owned files, one acceptance command each. Stack ids are PLAN's C ids (stacks.md's S ids renamed). TypeScript units are done when `bunx turbo run lint typecheck` stays green as well. Rust units also keep `cargo clippy` green; Go units run from `D:\dev\forgejo`; JetBrains commands run from `plugins/jetbrains`.

**Recorded amendments** (hunks in files that other units own):

| Owning unit and file | Hunk | Made by |
|---|---|---|
| U50 `crates/asc/src/cli/api.rs` | the `--stdio` flag dispatching to `crate::bridge` | P01a |
| U73 `proto/asc/v1/workspace.proto`, `docs/plugin-api.md` | `rpc ReadFile(WsReadFileRequest) returns (stream ReadFileResponse)` with `oneof at { rev_path, file_id, stage }`; one doc row | P02 |
| C14 `crates/asc/src/stack/cli/reorder.rs` | `--todo <file>` dispatching to `reorder_todo.rs` | P03 |
| M02 `proto/asc/v1/cli.proto` | `StackTodo { repeated TodoItem items }` (`pick`, `fold`, `drop`, `reword`, ordered bottom to top) | P03 |
| M02 `proto/asc/v1/cli.proto` | `--json` result messages (each an `OpResult` with typed fields) for `restore`, `switch`, `cherry-pick`, `revert`, `clone`, `worktree add\|remove\|list`, `stash`, `fetch`, `pull`, `push`; SPEC line 1404 defines no `--json` for them today, and the bridge's `Run` needs one | P45 |
| M02 `proto/asc/v1/cli.proto` | `SplitSpec { repeated SplitPiece pieces }`, `SplitPiece { string message; bytes patch; repeated bytes whole_files }` (git-apply patch per piece; the last piece takes the remainder) | P46 |
| C13 `crates/asc/src/stack/cli/split.rs` | `--hunks <spec>` dispatching to `split_hunks.rs`; the interactive `--by-hunk` is unchanged and never run by a plugin | P46 |
| SPEC §11.2 `--json` list (line 1410) | add the verbs of P45 | P45 |
| CLI alias resolution (owner named when folded into PLAN) | honour `ASC_NO_ALIASES=1` | P01b |

| ID | Title | Package | Depends | Owns | Accept |
|---|---|---|---|---|---|
| P01a | `bridge.proto` and `asc api --stdio`: framing, routing via the M09 backend, `Hello` (features), `Events` | asc, asc-rpc | U50, U52, M02, M09 | `proto/asc/v1/bridge.proto`, `crates/asc/src/bridge/{mod.rs,frame.rs,route.rs}`, `crates/asc/tests/bridge.rs` | `cargo test -p asc --test bridge && buf lint proto` |
| P01b | `Bridge.Run`: per-verb flag schema, argv rules of §3.5.1, rebuilt environment, no stdin, typed `OpResult` | asc | P01a, P45 | `crates/asc/src/bridge/{run.rs,argv_schema.rs}`, `crates/asc/tests/bridge_run.rs` | `cargo test -p asc --test bridge_run` |
| P01c | `Bridge.Token`/`StoreToken`: non-interactive credential calls, `NOT_FOUND`, `NO_CREDENTIAL_HELPER`, `ASC_TOKEN` precedence | asc | P01a | `crates/asc/src/bridge/auth.rs`, `crates/asc/tests/bridge_auth.rs` | `cargo test -p asc --test bridge_auth` |
| P02 | `Workspace.ReadFile` (rev, file id, stage set; offline from the local store) | asc | U73, C31 | `crates/asc/src/daemon/plugin_api_read.rs`, `crates/asc/tests/plugin_api_read.rs` | `cargo test -p asc --test plugin_api_read` |
| P03 | `asc reorder --todo <file>`: one-op stack edit (pick, fold, drop, reword) | asc | C14, C13, C15, M02 | `crates/asc/src/stack/cli/reorder_todo.rs`, `crates/asc/tests/stack_reorder_todo.rs` | `cargo test -p asc --test stack_reorder_todo` |
| P04 | Forgejo read-only asc RPC proxy `/-/asc/rpc/*` (gRPC-Web in, raw passthrough to asc, method allowlist, repo permission) | forgejo | K06, K07 | `routers/web/repo/asc_rpc_proxy.go`, `routers/web/repo/asc_rpc_proxy_test.go` | `go test ./routers/web/repo/... -run TestAscRpcProxy` |
| P05 | Forgejo Actions REST for plugins: runs and jobs by `asc_change`, ranged and tailed job logs (reuses the M24 services) | forgejo | K14, C40, M24 | `routers/api/v1/repo/action_asc.go`, `routers/api/v1/repo/action_asc_test.go` | `go test ./routers/api/v1/repo/... -run TestAscActionsAPI` |
| P06 | Forgejo OAuth public client `asc-ide` (PKCE; `vscode://`, `cursor://`, `vscodium://` redirects and loopback redirects on any port) | forgejo | M26 | `routers/web/auth/oauth_asc_ide.go`, `routers/web/auth/oauth_asc_ide_test.go` | `go test ./routers/web/auth/... -run TestAscIdeClient` |
| P07 | Forgejo `asc-ui` islands: vendored bundle, mount partial, version pin | forgejo | P04, P17, P18 | `public/assets/asc-ui/**`, `templates/base/asc_ui.tmpl`, `modules/ascui/{version.go,version_test.go}` | `go test ./modules/ascui/...` |
| P08 | Monorepo tooling: Bun workspaces, Turbo pipeline, tsconfig, oxlint, Biome, CI workflow | root | – | `package.json`, `bun.lock`, `turbo.json`, `tsconfig.base.json`, `.oxlintrc.json`, `biome.json`, `.forgejo/workflows/ts.yml` | `bun install --frozen-lockfile && bunx turbo run lint typecheck` |
| P09 | `@asc/asc-client`: codegen and the `bridgeNode` transport (restart, cancel, streams) | asc-client-ts | P08, P01a, P01b, P01c, U41, C17, G09, B07, I12, M02 | `proto/buf.gen.ts.yaml`, `packages/asc-client-ts/{package.json,src/index.ts,src/ids.ts,src/transport/{frame.ts,bridge-node.ts}}`, `packages/asc-client-ts/test/bridge.test.ts` | `bunx turbo run test --filter=@asc/asc-client -- test/bridge.test.ts` |
| P10 | `@asc/asc-client`: `relay`/`host` transports, `allowlist.json` and `gen-allowlist.ts` (fails on any unclassified RPC, verb flag or path), `grpcWeb` | asc-client-ts | P09 | `packages/asc-client-ts/src/transport/{relay.ts,host.ts,grpc-web.ts,allowlist.json}`, `packages/asc-client-ts/scripts/gen-allowlist.ts`, `packages/asc-client-ts/test/relay.test.ts` | `bunx turbo run test --filter=@asc/asc-client -- test/relay.test.ts` |
| P11 | `@asc/asc-client`: Forgejo REST client (types generated from the fork's swagger) | asc-client-ts | P09, C41b, P05 | `packages/asc-client-ts/src/forgejo/**`, `packages/asc-client-ts/scripts/gen-forgejo.ts`, `packages/asc-client-ts/test/forgejo.test.ts` | `bunx turbo run test --filter=@asc/asc-client -- test/forgejo.test.ts` |
| P12 | `@asc/asc-fixtures`: deterministic scenarios (§6.2) | asc-fixtures | P08, C10, C11, C13, C14, C15, C24, C26, B05, U54w | `packages/asc-fixtures/**` | `bunx turbo run test --filter=@asc/asc-fixtures` |
| P13 | `@asc/asc-ui` core: `Platform`, theme tokens, state, ops queue with the caps of §3.12, preview appliers | asc-ui | P10 | `packages/asc-ui/{package.json,src/platform.ts,src/theme/**,src/state/**,src/ops/**}`, `packages/asc-ui/test/ops.test.ts` | `bunx turbo run test --filter=@asc/asc-ui -- test/ops.test.ts` |
| P14 | `@asc/asc-ui-devhost` (Hono) with the mock Forgejo, and the Playwright harness | asc-ui-devhost | P13, P11, P12 | `packages/asc-ui-devhost/**`, `packages/asc-ui/playwright.config.ts`, `packages/asc-ui/e2e/smoke.spec.ts` | `bunx turbo run test:e2e --filter=@asc/asc-ui -- e2e/smoke.spec.ts` |
| P15 | `asc-ui` smartlog: stack rows, uncommitted node, drag reparent (move), cloud drafts, hide, uncommit/unamend/abandon menu, disabled reasons | asc-ui | P14, C08, C49, C14, C51, B05, B15, B16 | `packages/asc-ui/src/views/smartlog/**`, `packages/asc-ui/e2e/smartlog.spec.ts` | `bunx turbo run test:e2e --filter=@asc/asc-ui -- e2e/smartlog.spec.ts` |
| P16 | `asc-ui` stack editor (reorder, fold, drop, reword in one op) | asc-ui | P15, P03, C13 | `packages/asc-ui/src/views/stack-editor/**`, `packages/asc-ui/e2e/stack-editor.spec.ts` | `bunx turbo run test:e2e --filter=@asc/asc-ui -- e2e/stack-editor.spec.ts` |
| P17 | `asc-ui` commit graph (virtualised, canvas lanes, minimap from the page header, filters, keyboard, cherry-pick/revert menu items) with the 100,000-row performance test | asc-ui | P14, C29, C30c, C31 | `packages/asc-ui/src/views/graph/**`, `packages/asc-ui/e2e/graph.spec.ts`, `packages/asc-ui/e2e/graph-perf.spec.ts` | `bunx turbo run test:e2e --filter=@asc/asc-ui -- e2e/graph.spec.ts e2e/graph-perf.spec.ts` |
| P18 | `asc-ui` commit details and compare | asc-ui | P17, C30b, C19 | `packages/asc-ui/src/views/details/**`, `packages/asc-ui/src/views/compare/**`, `packages/asc-ui/e2e/details.spec.ts` | `bunx turbo run test:e2e --filter=@asc/asc-ui -- e2e/details.spec.ts` |
| P19 | `asc-ui` review panel (navigator, versions, interdiff, badges, comment list, approve, merge) | asc-ui | P14, P11, C20, C21, C50 | `packages/asc-ui/src/views/review/**`, `packages/asc-ui/e2e/review.spec.ts` | `bunx turbo run test:e2e --filter=@asc/asc-ui -- e2e/review.spec.ts` |
| P20 | `asc-ui` inbox (sections, pin, snooze) | asc-ui | P14, P11, C41b | `packages/asc-ui/src/views/inbox/**`, `packages/asc-ui/e2e/inbox.spec.ts` | `bunx turbo run test:e2e --filter=@asc/asc-ui -- e2e/inbox.spec.ts` |
| P21 | `asc-ui` Actions runs and logs, affected list, project graph | asc-ui | P14, P11, P05, G09 | `packages/asc-ui/src/views/{actions,affected,projects}/**`, `packages/asc-ui/e2e/ci.spec.ts` | `bunx turbo run test:e2e --filter=@asc/asc-ui -- e2e/ci.spec.ts` |
| P22 | `asc-ui` visual file history and worktrees tab | asc-ui | P14, C31 | `packages/asc-ui/src/views/{history,worktrees}/**`, `packages/asc-ui/e2e/history.spec.ts` | `bunx turbo run test:e2e --filter=@asc/asc-ui -- e2e/history.spec.ts` |
| P23 | `asc-ui` search (exact, regex, symbol, semantic; scope picker; coverage) | asc-ui | P14, I16, I18 | `packages/asc-ui/src/views/search/**`, `packages/asc-ui/e2e/search.spec.ts` | `bunx turbo run test:e2e --filter=@asc/asc-ui -- e2e/search.spec.ts` |
| P24 | VS Code skeleton: manifest, detection, activation, bridge lifecycle, `git.enabled` prompt, minimum-feature check, Bun bundle, test harness, activation budgets | vscode | P09, P12 | `plugins/vscode/{package.json,scripts/bundle.ts,.vscode-test.mjs,src/extension.ts,src/bridge.ts,src/repo.ts}`, `plugins/vscode/test/activation.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep activation` |
| P25 | VS Code SCM provider, `asc:` file system, quick diff, file decorations | vscode | P24, P02 | `plugins/vscode/src/{scm/provider.ts,fs/ascfs.ts,decorations.ts}`, `plugins/vscode/test/scm.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep scm` |
| P26a | VS Code create, amend, reword, absorb, stage ranges, discard, undo/redo | vscode | P25, C11, C12, C15, P45 | `plugins/vscode/src/scm/{commands.ts,stage-ranges.ts}`, `plugins/vscode/test/commit.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep commit` |
| P26b | VS Code status bar (`↓N` from `Stacks`), uncommit/unamend/abandon, stash, fetch/pull/push (git-remote mode), add to `.ascignore` | vscode | P26a, C09, C51, P45 | `plugins/vscode/src/scm/{statusbar.ts,stash.ts,remote.ts,ignore.ts}`, `plugins/vscode/test/scm-extra.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep scm-extra` |
| P27 | VS Code inline blame, hover, CodeLens, heatmap | vscode | P24, C31, C30d | `plugins/vscode/src/blame/**`, `plugins/vscode/test/blame.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep blame` |
| P28a | VS Code webview hosts for `asc-ui` (smartlog view, panels, serializers), CSP with `font-src`, host-side avatar fetch | vscode | P24, P15, P17 | `plugins/vscode/src/webview/{host.ts,panels.ts,csp.ts,avatar.ts}`, `plugins/vscode/test/webview.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep webview` |
| P28b | VS Code relay: `allowlist.json` enforcement for RPC, `Run` and Forgejo, native confirmations, token attachment | vscode | P28a, P10, P01b | `plugins/vscode/src/webview/{relay.ts,confirm.ts,forgejo-relay.ts}`, `plugins/vscode/test/relay.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep relay` |
| P29 | VS Code trees: Worktrees, Op log (debounced), File History (file and line mode), revision navigation | vscode | P25, C15, C30a, C31 | `plugins/vscode/src/views/{worktrees.ts,oplog.ts,file-history.ts}`, `plugins/vscode/src/revnav.ts`, `plugins/vscode/test/views.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep views` |
| P30 | VS Code conflicts (merge editor or markers, mark resolved, continue, abort) | vscode | P25, C10, C31 | `plugins/vscode/src/conflicts.ts`, `plugins/vscode/test/conflicts.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep conflicts` |
| P31a | VS Code review: comment controller on `asc:` diffs (thread caps of §3.12), stack checkout | vscode | P28b, P19, C26 | `plugins/vscode/src/review/{comments.ts,checkout.ts}`, `plugins/vscode/test/review.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep review` |
| P31b | VS Code Inbox tree and the status-bar count with its own poll | vscode | P28b, P20 | `plugins/vscode/src/views/inbox.ts`, `plugins/vscode/src/inbox-count.ts`, `plugins/vscode/test/inbox.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep inbox` |
| P32 | VS Code Actions and Affected trees, `asc-actions:` log documents, "Run target" task provider | vscode | P28b, P21, U73, G09 | `plugins/vscode/src/views/{actions.ts,affected.ts}`, `plugins/vscode/src/tasks.ts`, `plugins/vscode/src/fs/actions-log.ts`, `plugins/vscode/test/ci.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep ci` |
| P33a | VS Code auth provider: `asc-ide` OAuth, `StoreToken`, `SecretStorage` fallback | vscode | P24, P06, P01c | `plugins/vscode/src/auth.ts`, `plugins/vscode/test/auth.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep auth` |
| P33b | VS Code MCP registration (VS Code and Cursor), no token injection | vscode | P24, P50, M09, M20 | `plugins/vscode/src/mcp.ts`, `plugins/vscode/test/mcp.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep mcp` |
| P34 | VS Code search quick pick and search panel | vscode | P28b, P23 | `plugins/vscode/src/search.ts`, `plugins/vscode/test/search.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep search` |
| P35a | VS Code packaging: `vsce package`, README, CHANGELOG, icon | vscode | P24 | `plugins/vscode/{README.md,CHANGELOG.md,icon.png}` | `bunx turbo run package --filter=./plugins/vscode` |
| P35b | VSIX smoke test on VSCodium and VS Code, and the Cursor/VSCodium engine spike | vscode | P35a, P25 | `plugins/vscode/scripts/smoke-vsix.ts`, `plugins/vscode/test/smoke/**` | `bun plugins/vscode/scripts/smoke-vsix.ts --editor vscodium` |
| P35c | VS Code release workflow (`vsce` and `ovsx` from Forgejo Actions) and its dry-run checker | vscode | P35b, P44a | `.forgejo/workflows/vscode-release.yml`, `plugins/vscode/scripts/release-check.ts` | `bun plugins/vscode/scripts/release-check.ts` (workflow schema, `vsce ls`, token-less argument checks; the real publish is verified at the first tag, §8) |
| P36 | JetBrains skeleton: Gradle, protobuf-java from `proto/`, `AscBridgeService`, Turbo wrapper, `verifyPlugin` | jetbrains | P08, P01a, P01b, P01c | `plugins/jetbrains/{build.gradle.kts,settings.gradle.kts,gradle.properties,package.json}`, `plugins/jetbrains/src/main/resources/META-INF/plugin.xml`, `plugins/jetbrains/src/main/kotlin/asc/asc/bridge/**`, `plugins/jetbrains/src/test/kotlin/asc/asc/bridge/**` | `./gradlew test --tests 'asc.asc.bridge.*' verifyPlugin` |
| P37a | JetBrains `AscVcs`, root checker, mapping activity, unregistered-root suppression | jetbrains | P36, P52 | `plugins/jetbrains/src/main/kotlin/asc/asc/vcs/{AscVcs,AscRootChecker,AscMappingActivity}.kt`, `plugins/jetbrains/src/test/kotlin/asc/asc/vcs/RootsTest.kt` | `./gradlew test --tests 'asc.asc.vcs.RootsTest'` |
| P37b | JetBrains `ChangeProvider` and `Watch` → dirty scopes | jetbrains | P37a, P02 | `plugins/jetbrains/src/main/kotlin/asc/asc/vcs/{AscChangeProvider,AscWatchService}.kt`, `plugins/jetbrains/src/test/kotlin/asc/asc/vcs/ChangesTest.kt` | `./gradlew test --tests 'asc.asc.vcs.ChangesTest'` |
| P37c | JetBrains content revisions and `DiffProvider` | jetbrains | P37a, P02 | `plugins/jetbrains/src/main/kotlin/asc/asc/vcs/{AscContentRevision,AscDiffProvider}.kt`, `plugins/jetbrains/src/test/kotlin/asc/asc/vcs/DiffTest.kt` | `./gradlew test --tests 'asc.asc.vcs.DiffTest'` |
| P38 | JetBrains checkin (create, amend, create and submit), rollback, `.ascignore` provider | jetbrains | P37b, P37c, P53, C11, C24, P45 | `plugins/jetbrains/src/main/kotlin/asc/asc/vcs/{AscCheckinEnvironment,AscRollbackEnvironment,AscSubmitExecutor,AscIgnoreProvider}.kt`, `plugins/jetbrains/src/test/kotlin/asc/asc/vcs/CheckinTest.kt` | `./gradlew test --tests 'asc.asc.vcs.CheckinTest'` |
| P39 | JetBrains annotate, file history, history for selection | jetbrains | P37b, C30a, C31 | `plugins/jetbrains/src/main/kotlin/asc/asc/vcs/{AscAnnotationProvider,AscHistoryProvider}.kt`, `plugins/jetbrains/src/test/kotlin/asc/asc/vcs/HistoryTest.kt` | `./gradlew test --tests 'asc.asc.vcs.HistoryTest'` |
| P40 | JetBrains merge provider | jetbrains | P37c, C31 | `plugins/jetbrains/src/main/kotlin/asc/asc/vcs/AscMergeProvider.kt`, `plugins/jetbrains/src/test/kotlin/asc/asc/vcs/MergeTest.kt` | `./gradlew test --tests 'asc.asc.vcs.MergeTest'` |
| P41a | JetBrains JCEF tool window for `asc-ui` (resource handler, relay, theme, avatar fetch) | jetbrains | P36, P10, P48, P54, P15, P17, P19 | `plugins/jetbrains/src/main/kotlin/asc/asc/ui/{AscToolWindowFactory,AscBrowser,AscRelay,AscTheme,AscAvatar}.kt`, `plugins/jetbrains/src/test/kotlin/asc/asc/ui/RelayTest.kt` | `./gradlew test --tests 'asc.asc.ui.*'` |
| P41b | JetBrains status-bar widget and stack actions (up, down, restack, sync, submit, absorb, undo, redo) | jetbrains | P41a, C09 | `plugins/jetbrains/src/main/kotlin/asc/asc/ui/{AscStatusBarWidgetFactory,AscActions}.kt`, `plugins/jetbrains/src/test/kotlin/asc/asc/ui/ActionsTest.kt` | `./gradlew test --tests 'asc.asc.ui.ActionsTest'` |
| P42 | JetBrains sign-in (OAuth loopback with `state`/PKCE, PAT, `PasswordSafe` fallback) and "Configure agents" (`asc mcp install`) | jetbrains | P36, P06, P55, P01c, M09 | `plugins/jetbrains/src/main/kotlin/asc/asc/auth/**`, `plugins/jetbrains/src/test/kotlin/asc/asc/auth/AuthTest.kt` | `./gradlew test --tests 'asc.asc.auth.*'` |
| P43a | JetBrains UI tests (IDE Starter + Driver) | jetbrains | P38, P39, P40, P41a | `plugins/jetbrains/src/uiTest/**` | `./gradlew uiTest` |
| P43b | JetBrains Marketplace packaging and publishing workflow | jetbrains | P43a, P44b | `.forgejo/workflows/jetbrains-release.yml`, `plugins/jetbrains/scripts/release-check.ts` | `./gradlew buildPlugin verifyPlugin signPlugin` (test certificate from env) then `bun plugins/jetbrains/scripts/release-check.ts` |
| P44a | VS Code budget gate: activation time, heap probe (test-only extension-host window), bridge RSS, bundle sizes, graph fps; fails on regression against `bench/plugins/baseline.vscode.json` | bench, CI | P24, P17, P28a | `bench/plugins/{collect.ts,heap-probe/**,baseline.vscode.json}`, `.forgejo/workflows/plugin-budgets.yml` | `bun bench/plugins/collect.ts --target vscode --check` |
| P44b | JetBrains budget gate: activation time, JVM heap delta, bridge RSS, jar size | bench, CI | P41a, P44a | `bench/plugins/{jetbrains.ts,baseline.jetbrains.json}` | `bun bench/plugins/collect.ts --target jetbrains --check` |
| P45 | `--json` forms and typed `OpResult`s for `restore`, `switch`, `cherry-pick`, `revert`, `clone`, `worktree`, `stash`, `fetch`, `pull`, `push` (M02 amendment) | asc, asc-rpc | M02, U50, C11 | `crates/asc/src/cli/json_verbs.rs`, `crates/asc/tests/plugin_json_verbs.rs` | `cargo test -p asc --test plugin_json_verbs` |
| P46 | `asc split --hunks <spec>`: non-interactive split by patch per piece | asc | C13, M02 | `crates/asc/src/stack/cli/split_hunks.rs`, `crates/asc/tests/stack_split_hunks.rs` | `cargo test -p asc --test stack_split_hunks` |
| P47a | VS Code stack commands: up/down/top/bottom with the fork quick pick, `scm/title` restack, sync and submit buttons, share | vscode | P26b, C09, C32 | `plugins/vscode/src/stack/{nav.ts,actions.ts,share.ts}`, `plugins/vscode/test/stack.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep stack` |
| P47b | VS Code cloud status item and "Show cloud drafts" toggle | vscode | P26b, B15, B16 | `plugins/vscode/src/stack/cloud.ts`, `plugins/vscode/test/cloud.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep cloud` |
| P47c | VS Code graph context-menu wiring: cherry-pick and revert with host-composed confirmations | vscode | P28b, P45 | `plugins/vscode/src/webview/graph-actions.ts`, `plugins/vscode/test/graph-actions.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep graph-actions` |
| P48 | JetBrains allowlist generator: Gradle task `generateAllowlist` from `allowlist.json` to `Allowlist.kt` | jetbrains | P36, P10 | `plugins/jetbrains/buildSrc/**`, `plugins/jetbrains/src/test/kotlin/asc/asc/ui/AllowlistTest.kt` | `./gradlew generateAllowlist test --tests 'asc.asc.ui.AllowlistTest'` |
| P49a | VS Code hydration guard: exclusions prompt, per-process hydration notification, no-hydration test | vscode | P24, P25 | `plugins/vscode/src/hydration.ts`, `plugins/vscode/test/hydration.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep hydration` |
| P49b | JetBrains hydration guard: index excludes, `.ascignore` provider wiring, notification | jetbrains | P37a | `plugins/jetbrains/src/main/kotlin/asc/asc/vcs/AscIndexGuard.kt`, `plugins/jetbrains/src/test/kotlin/asc/asc/vcs/HydrationTest.kt` | `./gradlew test --tests 'asc.asc.vcs.HydrationTest'` |
| P50 | Spike: MCP OAuth from the VS Code and Cursor clients against `/api/mcp` (M20 PRM, M26 client registration); decides PAT `inputs` fallback | vscode, forgejo | M20, M26, P24 | `plugins/vscode/spikes/mcp-oauth.md`, `plugins/vscode/test/spikes/mcp-oauth.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep spike-mcp-oauth` |
| P51 | Spike: features lost without `VcsLogProvider`; comments API survey | jetbrains | P36 | `plugins/jetbrains/spikes/vcslog/**`, `plugins/jetbrains/spikes/RESULTS-vcslog.md` | `./gradlew :spikes:vcslog:test` |
| P52 | Spike: Git4Idea and the synthetic `.git` root | jetbrains | P36 | `plugins/jetbrains/spikes/git4idea-root/**` | `./gradlew :spikes:git4idea-root:test` |
| P53 | Spike: partial-line commits (`arePartialChangelistsSupported`) | jetbrains | P36 | `plugins/jetbrains/spikes/partial-changelists/**` | `./gradlew :spikes:partial-changelists:test` |
| P54 | Spike: the `jcef` module and optional dependency across 2026.1 and 2026.2 | jetbrains | P36 | `plugins/jetbrains/spikes/jcef/**` | `./gradlew :spikes:jcef:verifyPlugin` |
| P55 | Spike: OAuth loopback (moved port, `state`, PKCE, variable-port redirect against P06) | jetbrains | P36, P06 | `plugins/jetbrains/spikes/loopback/**` | `./gradlew :spikes:loopback:test` |
| P56 | `asc-ui` split dialog (hunks and lines per piece) and submit dialog (draft/publish, reviewers, auto-merge, PR title and description) | asc-ui | P16, P11, P46 | `packages/asc-ui/src/views/{split,submit}/**`, `packages/asc-ui/e2e/split-submit.spec.ts` | `bunx turbo run test:e2e --filter=@asc/asc-ui -- e2e/split-submit.spec.ts` |

**Order.** P01a, P01c, P02, P03, P45 and P46 follow their Rust dependencies; P01b needs P45. P08–P14 can start as soon as P01a exists (P09 needs P01b and P01c). The VS Code release is P24–P35c, P44a, P47a–P47c, P49a and P50 plus the `asc-ui` views they embed (P15–P23, P56); it never waits for JetBrains. JetBrains: P36 needs only P08 and P01a–P01c, so the spikes P51–P55 and the provider units P37a–P40, P42, P48 and P49b need no `asc-ui` view and can start right after it (gated by their spikes: P37a by P52, P38 by P53, P42 by P55). P41a needs P15, P17, P19, P48 and P54; P41b and P43a need P41a. The JetBrains release gate is P44b. The Forgejo units P04–P07 run on the Go track after K07, K14 and M26.

### 7.1 asc API additions the plugins need (for SPEC)

| # | Addition | Used by | Owner to name when folded into SPEC |
|---|---|---|---|
| A1 | `WsStacksResponse.stacks[].behind_trunk` (trunk commits not in the stack base) | status bar `↓N` (§3.2) | §20.6 plugin API |
| A2 | First `CommitGraphResponse` page carries `total` rows and `ref_markers[]` (ref name, row index) | minimap (§3.5) | §20.6 |
| A3 | `Repo.ListRefs` (kind `tag`/`branch`, paged) | tags view, v1.x | §7 |
| A4 | `AffectedResponse.projects[].targets[<name>].argv` | "Run target" task (§3.7) | §12A.8 |
| A5 | `ASC_NO_ALIASES=1` disables `alias.*` resolution | bridge argv rules (§3.5.1) | §11 |
| A6 | `GET /user/asc/inbox?section=&limit=` returns `X-Total-Count` and honours `If-None-Match` | inbox count poll (§3.7) | C41b |
| A7 | `--end-of-options` accepted by every verb of `allowlist.json` (git 2.55 does; verify per asc verb) | bridge argv rules | §11 |
| A8 | `--json` forms for the verbs of P45, and `split --hunks <spec>` with `SplitSpec` (P46) | `Run` | §11.2, M02 |
| A9 | `HelloResponse.features` flags `plugin.v1`, `json-verbs.v1`, `reorder-todo.v1`, `split-hunks.v1` | minimum-feature check (§3.1) | §7 `bridge.proto` |

---

## 8. Risks and spikes

| Risk | Mitigation |
|---|---|
| `Bun.build` CJS output is experimental | the test-electron suite runs on the bundle; `scripts/bundle.ts` swaps to esbuild in one file |
| `_open.mergeEditor` is internal and unversioned | feature-detected; the marker fallback is fully functional |
| Cursor and VSCodium lag the VS Code engine | spike in P35b: install the VSIX on the current Cursor and VSCodium; lower `engines` only if a stable API used here is missing |
| JCEF unavailable (some Linux setups, remote IDEs) | a notice and the Forgejo island fallback (§4.2) |
| Bridge process crash | restart with backoff, `Watch` resumes from its token; the UI shows "reconnecting" |
| Kotlin and TypeScript disagree on the relay allowlist | one `allowlist.json`, generated into Kotlin at build time (P48); the bridge schema is tested against it (P01b) |
| A compromised webview or a bad argv reaches git options | the bridge, not the host, owns the flag schema; `--end-of-options`; no stdin; injection corpus in `bridge_run` (P01b) |
| IDE MCP clients cannot register with Forgejo (no DCR; CIMD is v1.x) | spike P50; the PAT `inputs` fallback of §3.8 ships if it fails |
| JetBrains degrades without `VcsLogProvider`, shows the synthetic `.git` as an unregistered root, lacks partial-line commit support, or has a different `jcef` module rule | spikes P51–P54; features listed in §2.6 and §4.4 |
| OAuth loopback port moves or the redirect is not accepted | spike P55; own `ServerSocket` fallback |
| Search and indexers hydrate a virtual workspace | exclusions, `asc: Search code`, per-process counters (§3.11, P49a/b) |
| Publishing to the Marketplace and Open VSX cannot be proven by a local command | `release-check.ts` covers everything but the token step; the first tagged release is watched by hand and recorded in the CHANGELOG |

---

## 9. Edits needed in SPEC.md, PLAN.md and other designs (not made here)

| File | Edit |
|---|---|
| SPEC §7 | Add `bridge.proto` (the `Bridge` service, framing, `RunRequest` without stdin, `HelloResponse.features`) and `Workspace.ReadFile` to the plugin API; note that ASC's plugins reach every RPC through `asc api --stdio`; add the API additions A1–A9 of §7.1. |
| SPEC §11.2 | `asc api --stdio`; `asc reorder --todo <file>`; `asc split --hunks <spec>`; the `--json` forms of P45 appended to the list at line 1410; `ASC_NO_ALIASES`. |
| SPEC §13.3 | Fork units P04–P07 (RPC proxy, Actions REST, `asc-ide` OAuth client, `asc-ui` islands). |
| SPEC §16.4 | The plugin budgets of §3.10 and the P44a/P44b gates. |
| SPEC §17 | The plugin test layers of §6. |
| PLAN | A new "§P IDE plugins track" with P01a–P56 (§7), the recorded amendments of §7 and the TypeScript rules (Bun, Turbo, oxlint, Biome, strict tsconfig). The workspace layout block gains `packages/` and `plugins/`. |
| stacks.md line 10 (fixed rules) | Replace "there is no IDE extension, so GitLens/GitKraken-style features are data APIs" with "ASC's plugins render these data APIs (`ide-plugins.md`)". |
| stacks.md line 139 (GitKraken comparison, row "Command palette, terminal links, repo tabs, profiles, Gitflow, multi-repo workspaces, work items, agent sessions") | Keep OUT, but change the reason from "These are IDE or desktop UI" to "GitKraken desktop features or products it deprecated; the IDE surfaces are built in `ide-plugins.md`". No row remains that marks IDE UI as OUT. |
| SPEC line 7 (user decisions, item 3) | Retitle "no IDE git-plumbing support" to "no reliance on IDEs' built-in git" and end with "ASC builds its own VS Code and JetBrains plugins (`research/ide-plugins.md`)". The rest of the sentence already says the plugins use asc's API. |
| SPEC line 32 (principle 6) | Replace "IDE git plumbing is not a target: ASC's IDE plugins use asc's API [audit: user decision No-IDE]" with "IDE git plumbing is not a target: ASC's own IDE plugins use asc's API (`research/ide-plugins.md`)". |
| SPEC line 65 (decisions table row) | Retitle "No IDE support: …" to "No IDE git plumbing; ASC IDE plugins on the plugin API: …" and add `research/ide-plugins.md` to the sections column. |
| SPEC line 2166 (§20 fixed rules) | Same replacement as stacks.md line 10. |
| SPEC lines 2561 and 2610, PLAN lines 13, 362, 592, 815 | Keep the "No-IDE" audit tags (they name the decision's history) and add "(superseded in part by `ide-plugins.md`: plugins are built; git plumbing still is not)" at the first mention in each file. |
| mcp.md placement rule and M26 | Add one sentence: IDE MCP clients register through the same OAuth path as other clients; P50 decides whether `asc-mcp` or a second public client covers them. |

---

## Review notes

One line per critique issue. "Accepted" means the fix is in this file at the section named; a rejection or partial acceptance states its reason.

1. Accepted: relay is an allowlist of RPCs, verbs and (method, path) pairs, with modal confirmation for merge and review submit (§3.5, §3.5.1, ID4).
2. Accepted: the bridge owns a per-verb flag schema, rejects `-c`, `-C`, `--exec`, `--exec-path` and aliases, inserts `--end-of-options`, and takes no stdin (§3.5.1, P01b).
3. Accepted: "every read RPC" is replaced by the full `allowlist.json` in §3.5.1, with `Stage` and `RequestMerge` as explicit writes and `Prefetch`, `Journal`, `Stats`, `ClearJournal` in the never list.
4. Accepted: non-interactive credential calls, `NO_CREDENTIAL_HELPER`, and a `SecretStorage`/`PasswordSafe` fallback passed as `ASC_TOKEN` (§5, P01c, P33a).
5. Partly accepted: token injection is dropped and the IDE's MCP OAuth is used (§3.8, P33b, P50). M20 accepts any Forgejo API token and M26's audience binding applies to the asc server's `/mcp`, so the old design was not a protocol error; it is dropped because a `write:repository` token must not reach an agent channel.
6. Partly rejected: `asc stack merge` is kept, because it exists (SPEC §20.7 line 2461, stacks.md:257, unit S28/C28). The rest is accepted: M02 `--json` amendment (P45), `sl --cloud --user <me>`, and a non-interactive split (`split --hunks`, P46).
7. Accepted: P32 now depends on U73 and G09; C09, C51, C14, B05, C10, C26 and C32 added where named; share is owned by P47a; P44 is split into P44a and P44b; the order paragraph and P36's dependencies now agree.
8. Accepted: up/down quick pick, `scm/title` buttons and share (P47a), cloud status item (P47b), graph cherry-pick and revert (P47c, P17), Kotlin allowlist generator (P48).
9. Accepted: P01, P28, P31, P37, P26, P33, P35, P41, P43 and P44 are split; P35b adds a runnable VSCodium smoke command; P44a measures heap in a test-only window. Publishing itself cannot be accepted by a local command, so P35c and P43b check everything but the token step and the first tag is verified by hand (§8).
10. Accepted: spikes P51–P55, the degraded-feature list (§4.4), `jcef` as an optional dependency until verified (§4), loopback `state`/PKCE and variable port (§5).
11. Accepted: §3.11 and ID14 (exclusions, `asc: Search code`, per-process counters, tests in P49a and P49b).
12. Accepted: rows for stash, tags, fetch/pull/push, `.ascignore`, signing, submit options, PR title and description, split dialog (§2.1, §2.2); JetBrains v1 limits with reasons (§2.6); uncommit moved into JetBrains v1 through the `asc-ui` Stack tab. A signing settings UI and tags view are out of scope or v1.x with the reason stated.
13. Accepted: `↓N` source (A1), inbox count poll (A6), minimap source (A2), `Hello.features` and `search.v1` (§1, §3.7), minimum version as features (§3.1), "Run target" as a task provider (A4).
14. Accepted: §3.12 caps and debounce, ID15, op log debounce in §3.4.
15. Accepted: `font-src ${cspSource}` and host-side avatar proxy (§3.5).
16. Partly accepted: SPEC lines 7 and 32 already say git plumbing only and name the plugin API, so those rewrites only retitle; lines 65 and 2166, stacks.md:10 and stacks.md:139 need real rewrites (§9). No SPEC, PLAN, mcp.md or stacks.md file is edited by this review.
