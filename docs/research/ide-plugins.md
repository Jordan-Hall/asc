# ASC-built IDE plugins for asc

Status: design for the checklist decision "ASC-built IDE plugins" (2026-10-22). Not normative until folded into SPEC.md and PLAN.md; §9 lists the edits those files need.

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
| ID3 | **Reads are RPCs. Writes are asc verbs run through `Bridge.Run`**, which returns the typed `OpResult` of `cli.proto` (M02). The bridge spawns `asc <verb> --json` and decodes it with `asc_rpc::json`, the single codec. | The same rule as mcp.md M-D7. Hooks, the op log, the ref chokepoint and locks are reused exactly. TypeScript and Kotlin only ever see protobuf, never JSON text and never human output. |
| ID4 | **Webviews hold no tokens and no process access.** `asc-ui` calls a `Platform` interface. Its RPCs go through a host relay that has a method allowlist. Destructive verbs need a native confirmation dialog in the host. Forgejo REST calls from a webview are relayed by the host, which adds the token. | This is ISL's security model. A compromised webview cannot push, land or read credentials. |
| ID5 | **No proposed VS Code APIs.** The commit graph is a webview, not `scmHistoryProvider`. Timeline is our own history view. The action button becomes `acceptInputCommand` plus title buttons. | Marketplace and Open VSX builds cannot enable proposals (§3.9). GitLens ships the same way. |
| ID6 | **Replace built-in git, and be inert elsewhere.** The plugins claim only asc workspaces: a root `.git` gitfile whose gitdir contains `asc-workspace` (SPEC §9.4). VS Code: the workspace setting `git.enabled: false`, offered once and checked into ASC. JetBrains: `vcs.xml` maps the roots to `asc`. A non-asc folder costs one `stat` and nothing else. | The built-in git would otherwise drive asc through the shim and duplicate every view. A plain git repo in the same window keeps working. |
| ID7 | **Branchless first.** The status bar shows the change id, the stack position and the cloud state, not a branch. The primary action is "Create change" (`asc create --no-branch`), not "Commit to branch". Branches appear only as labels. The trunk is `master`. | Checklist decision "branchless by default", plus BD1–BD3. |
| ID8 | **Never gate a local feature behind an account.** Status, commit, stacks, graph, blame, history, undo, conflicts and affected all work signed out and offline. Only Forgejo-backed views ask for sign-in. | The top complaint against GitLens 19 and GitKraken. |
| ID9 | **Event-driven only.** The `Watch` stream (paths, HEAD, ref, stage, op, switch, `KIND_STACK`, `KIND_CLOUD`) is the only refresh trigger. There is no FileSystemWatcher on `**` and no timer polling. The one exception is the no-daemon fallback (§5). | The cost of a refresh is then O(changed paths), with no repo walks. |
| ID10 | **Optimistic operation queue.** Each `asc-ui` mutation has a pure preview applier over the `Stacks` model. The UI shows the predicted state at once, queues verbs in order and keeps the preview until `Watch` confirms. `--dry-run` predicts conflicts before a drag is dropped. | ISL's lifecycle (preview, queued, running, optimistic, done) is why it feels instant over a CLI. |
| ID11 | **JetBrains: native VCS providers, but no `VcsLogProvider`.** `readAllHashes` loads the whole history, which breaks the no-whole-repo-scan rule. The graph is `asc-ui` in JCEF, as jj-idea builds its own graph. | Native providers give the Commit window, gutters, Annotate, Show History, the merge tool and Rollback for free. |
| ID12 | **AI stays out of the plugins.** Agents get asc through MCP. The plugin registers `asc mcp` and Forgejo `/api/mcp` with the IDE. | stacks D12 and mcp.md M-D1. No upsell surface. |
| ID13 | **Bun for all TypeScript** (install, build, test, scripts), Turborepo for orchestration, Hono only for the `asc-ui` dev host and the mock Forgejo. JetBrains is Gradle (Kotlin DSL), wrapped as Turbo tasks. | One toolchain. Hono is small and runs on Bun. |

---

## 1. Repo layout

The TypeScript and Kotlin code lives in this repo next to the Rust crates, so `proto/` is the single source.

```
package.json               Bun workspaces ["packages/*", "plugins/*"], packageManager bun@<pinned>,
                           devDeps: turbo, typescript, oxlint (ASC anti-slop rules), @biomejs/biome (format), @bufbuild/buf
turbo.json                 tasks: codegen → build → typecheck, lint, test, test:e2e, package
tsconfig.base.json         strict, noUncheckedIndexedAccess, exactOptionalPropertyTypes, verbatimModuleSyntax
proto/buf.gen.ts.yaml      protoc-gen-es v2 (target ts) → packages/asc-client-ts/src/gen/ (generated, gitignored, Turbo-cached)
proto/asc/v1/bridge.proto   BridgeFrame, Bridge service (Hello, Run, Token, StoreToken, Events)            (P01)
packages/asc-client-ts/     @asc/asc-client: gen/, transport/{bridge-node,relay,host,grpc-web}.ts, forgejo/, ids.ts
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

**Bridge protocol (`bridge.proto`, P01).**

```proto
message BridgeFrame { uint32 id = 1;
  oneof f { Call call = 2; bytes reply = 3; End end = 4; bool cancel = 5; } }
message Call { string method = 1;      // "asc.v1.Workspace/Status", "asc.v1.Changes/Stack", "asc.v1.Bridge/Run", …
  bytes request = 2; bool server_stream = 3; string root = 4; /* workspace root; selects ws_id, repo, remote */ }
message End { int32 code = 1; string message = 2; bytes error_detail = 3; /* asc.v1.ErrorDetail */ }
service Bridge {
  rpc Hello(HelloRequest) returns (HelloResponse);   // asc version, features, roots → {ws_id, repo, remote kind, forge URL,
                                                     //  branchless, daemon|server|forge state}
  rpc Run(RunRequest) returns (stream RunEvent);     // argv (asc verb + --json added), stdin; events: stderr line, OpResult, exit
  rpc Token(TokenRequest) returns (TokenResponse);   // forge host → bearer; host-only, never relayed to a webview
  rpc StoreToken(StoreTokenRequest) returns (StoreTokenResponse);  // git credential approve, as `asc auth login`
  rpc Events(EventsRequest) returns (stream BridgeEvent);          // connectivity changes, daemon restarts
}
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
| Discard | all | resource menu with a modal confirmation | `RollbackEnvironment` | Run `restore --staged --worktree` | Y | Y |
| Quick-diff gutter against `HEAD` | VG JB | `quickDiffProvider` → `asc:` URIs | `DiffProvider`/`ContentRevision` (line status tracker) | `Workspace.ReadFile` (P02) | Y | Y |
| Explorer and tab badges | VG JB | `FileDecorationProvider`, targeted invalidation | `FileStatus` from `ChangeProvider` | `Status` | Y | Y |
| Create change, amend, reword | all | input box + `acceptInputCommand` (Ctrl+Enter = create), title buttons | `CheckinEnvironment` + `AmendCommitAware`; executors "Create change", "Create and submit" | Run `create --no-branch`, `modify`, `reword` | Y | Y |
| Absorb with preview | GT ISL | SCM title button → preview quick pick → apply | Commit window action | Run `absorb -n`, then `absorb -f` | Y | Y |
| Hook output (husky, commitlint) | GK | Output channel; a failure shows stderr in a notification | VCS console | `RunEvent.stderr` | Y | Y |
| Uncommit, unamend | ISL | stack menu | action | Run `uncommit`, `unamend` | Y | – |
| Multi-root and nested worktrees | GK GL | one `SourceControl` per asc root | one VCS root mapping per root | `Bridge.Hello` per root | Y | Y |

### 2.2 Stacks and branchless (Graphite, ISL)

| Feature | Seen in | VS Code surface | JetBrains surface | asc API | VSC v1 | JB v1 |
|---|---|---|---|---|---|---|
| Smartlog: drafts relative to `master` with PR state, checks, needs-restack, divergent, frozen, worktree, version | GT ISL | `asc-ui` smartlog in a webview view in the SCM container | `asc-ui` tab "Stack" | `Workspace.Stacks`, `Changes.Stack`/`ResolveChanges`, `Watch KIND_STACK` | Y | Y |
| Uncommitted node with per-file checkboxes → "create change" or "modify ‹change›" | GT | smartlog | smartlog | `Stage`, Run `create`/`modify --into` | Y | Y |
| Drag a change onto a new parent (moves descendants) | GT ISL GK GG | `asc-ui` drag and drop with predicted-conflict badge | same | Run `move --onto -n`, then `move` | Y | Y |
| Stack editor: reorder, fold, drop, split, reword, committed in one op | ISL GK GL | `asc-ui` stack editor | same | Run `reorder --todo` (P03), `split` | Y | Y |
| Restack, sync, continue, abort | GT ISL | smartlog buttons, commands, status-bar "needs restack" | actions | Run `restack`, `sync`, `continue`, `abort` | Y | Y |
| Navigate up, down, top, bottom | GT | commands with keybindings; at a fork, a quick pick of the candidates | actions | Run `up`/`down` (exit 2 → candidates) | Y | Y |
| Submit stack, update PRs as new versions | GT | smartlog, command | smartlog | Run `submit [-s]` | Y | Y |
| Merge stack (queue) | GT | smartlog, review panel | same | `Changes.RequestMerge` | Y | Y |
| Get a stack (frozen), optionally into a new worktree | GT ISL | command with PR or change quick pick | action | Run `stack get [--worktree]` | Y | – |
| Hide and unhide drafts; other machines' drafts | ISL | smartlog menu, "Show cloud drafts" toggle | same | Run `hide`/`unhide`, `sl --cloud`; `Cloud.GetWorkspace`; `Watch KIND_CLOUD` | Y | Y |
| Cloud upload state | – | status bar item ("synced", "N pending", "offline") | status bar widget | `Bridge.Events`, `Watch KIND_CLOUD` | Y | Y |
| Undo and redo, op log | GK ISL | status-bar undo/redo, "Op log" tree | `asc-ui` tab | Run `undo`/`redo`, `op log` | Y | Y |
| Change versions and interdiff | ISL GT | `asc-ui` details | same | `Changes.Versions`, `Changes.InterDiff` | Y | Y |
| Optimistic queue (ID10) | ISL | `asc-ui` ops | same | Run + `Watch` | Y | Y |
| Branch labels (opt-in) | GK GL | labels on graph rows; "Name this change" | same | `Stacks.branches`, Run `create <branch>` | Y | Y |
| Share a snapshot link | – | command, copies `asc:<id>` and the web link | action | Run `share` | Y | – |

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
| Commit graph: lanes, minimap, scroll markers, column toggles, search, keyboard navigation, WIP rows per worktree | GK GL | `asc-ui` editor panel; compact graph in the sidebar view | `asc-ui` tab | `Workspace.CommitGraph` / `Repo.CommitGraph` (C29, C31) | Y | Y |
| Commit details: message, files, diff, change, PR | all | `asc-ui` details; files open in `vscode.changes` (multi-diff) | `asc-ui` details; native diff | `ReadChangeset`, `ResolveChanges`, `Diff` | Y | Y |
| Compare two revisions, changes or stacks | GK GL JB | `asc-ui` compare + `vscode.changes` | `asc-ui` + native diff | `Compare` (C30b), `Diff` | Y | Y |
| Search commits by message, author, file, code change | GK GL JB | quick pick + graph filter | `asc-ui` graph filter | `Log` `grep`/`author`/`paths`, pickaxe (C30c) | Y | Y |
| Worktrees: list, create, open, remove, dirty, ahead/behind | GK GL | tree view "Worktrees" | `asc-ui` tab | `Workspace.Worktrees`, Run `worktree add/remove` | Y | Y |
| Cherry-pick and revert from the graph | GK GL | graph context menu | same | Run `cherry-pick`/`revert` (exit code + `Status`) | Y | Y |

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
| Inline review comments in diff editors | GT | Comments API (`comments.createCommentController`) on `asc:` diff URIs | comment list in `asc-ui` | Forgejo reviews and comments API | Y | – |
| Approve, request changes, merge (stack guard 409) | GT | review panel, commands | review tab | Forgejo reviews and merge; `Changes.RequestMerge` | Y | Y |
| Check out a PR's stack | GT GK | command from the inbox or review | action | Run `stack get` | Y | Y |
| Actions: runs and jobs per change, log tail | GT (checks) | tree view "Actions"; logs as read-only `asc-actions:` documents | `asc-ui` tab | Forgejo Actions REST (P05) | Y | Y |
| Affected projects (uncommitted, change, stack) | – | tree view "Affected" | `asc-ui` tab | `Workspace.Affected` (`uncommitted`), `Projects.Affected` | Y | Y |
| Project graph | – | `asc-ui` panel | `asc-ui` tab | `Projects.ProjectGraph` | Y | – |
| Code search: exact and regex at any revision, change or stack | – | quick pick (streamed) + `asc-ui` results panel | `asc-ui` tab | `CodeIntel.Search` (I16, I18), fallback `Repo.Grep` | Y | Y |
| Symbol and semantic search | – | `asc-ui` search modes; `Go to asc symbol` command | `asc-ui` tab | `CodeIntel.Symbols`, `CodeIntel.Semantic`, `IndexStatus` | Y (when `search.v1` is advertised) | Y |
| MCP for agents | GL | `lm.registerMcpServerDefinitionProvider` (`asc mcp` stdio + Forgejo `/api/mcp`); Cursor: `vscode.cursor.mcp.registerServer` | action "Configure agents" → Run `mcp install` | M09, M20 | Y | Y |

**Out of scope:** AI commit messages and AI compose (ID12), cloud workspaces shared across a team (GitKraken), GitHub, GitLab and Jira integrations, LFS (No LFS decision), git-flow, submodule editing (submodules are read-only compositions, SPEC §10.6), paid tiers.

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
| `asc` binary | Setting `asc.path`, else `PATH`, else `$ASC_HOME/bin`. The bridge `Hello` gives the version; below the minimum, show "Update asc" and disable mutating commands. | – |
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
- Input box placeholder: "Message (Ctrl+Enter to create a change on ‹change›)". `acceptInputCommand` = `asc.create`. `scm/title`: Create, Amend, Absorb, Restack, Sync, Submit stack, Undo. `count` = the number of changed files.
- **`asc:` URIs:** `asc:/<path>?root=<ws>&rev=<sha>`, `…&stage=<0-3>` or `…&file=<FileId>`. They are served by `Workspace.ReadFile` (P02), which works offline from the local store and hydrates through the tiers. Immutable content (rev or file id) is cached in an LRU by id (32 MiB). Stage content is invalidated by `KIND_STAGE`.
- **Quick diff:** `quickDiffProvider.provideOriginalResource(uri)` returns `asc:` at `HEAD`. "Diff against stack parent" opens `vscode.diff(asc:parent, file)`, since a second quick-diff provider would be a proposed API.
- **Stage selected ranges or a hunk:** compute the hunks of `asc:HEAD` against the buffer in the host (the `diff` package, file-local), build a `git apply --cached` patch, then call `Workspace.Stage{patch}`.
- **Decorations:** one `FileDecorationProvider`. `onDidChangeFileDecorations` fires only with the URIs from the last `Status(paths)` delta. Colours use the `gitDecoration.*` theme colours so themes keep working.
- **Status bar:** `$(git-commit) kxyzq… · 2/5 · ↓3` (change, stack position, behind `master`), a cloud indicator, and "needs restack" as a warning item. Clicking it opens the smartlog.
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
| Inbox | Forgejo inbox | on focus, and every 60 s only while visible (ETag) |
| Actions | Forgejo runs for the current stack's PRs | every 5 s only while a run is in progress and the view is visible |
| Affected | `Workspace.Affected{uncommitted}` and per change | `KIND_PATHS` (debounced 1 s) |
| Op log | Run `op log` | `KIND_OP_FINISHED` |
| File History | `Workspace.Log{paths, follow, with_stats, limit: 50}` for the active editor | active editor change, `KIND_HEAD` |

### 3.5 `asc-ui` webviews: smartlog, graph, review, history

- **Hosts:** one `WebviewViewProvider` (`asc.smartlog`) in the SCM container, and `createWebviewPanel` editor tabs for the graph, review, visual history, project graph and search. Each panel restores through a `WebviewPanelSerializer` (`onWebviewPanel:*`). `retainContextWhenHidden` is false; `asc-ui` keeps its scroll and selection in `vscode.setState`.
- **Security:** a CSP with `default-src 'none'`, scripts and styles from `webview.cspSource` only, images from `webview.cspSource` and the forge origin (avatars), and `localResourceRoots` = the `asc-ui` dist.
- **Relay:** the webview's `asc-client` uses the `host` transport (`postMessage` of `{id, method, bytes}`). The extension relays frames to the bridge when `method` is on the allowlist: every read RPC of `Workspace`, `Repo`, `Changes`, `Projects`, `Cloud` and `CodeIntel`; `Bridge.Run` with a verb allowlist. `Token`, `StoreToken` and `Workspace.{Mount,Unmount,Switch,Dehydrate,Shutdown}` are never relayed. Destructive verbs (`abandon`, `hide`, `stack delete`, `submit`, `stack merge`, `restore`, `worktree remove`) need a native modal confirmation in the host first. Forgejo calls are relayed as `{path, method, body}` for `/api/v1/**` on the configured forge origin, with the token added by the host.
- **Theme:** `asc-ui` uses its own `--asc-*` tokens. The VS Code platform maps them from `--vscode-*` variables and the `vscode-light`/`vscode-dark`/`vscode-high-contrast` body classes.
- **Graph rendering:** rows come from `CommitGraph` pages of 200, with server-computed `column` and `edges`, so the client does no layout. DOM rows are virtualised (TanStack Virtual, overscan 20). One `<canvas>` draws the lanes for the visible window. At most 5,000 rows stay in memory (a ring keyed by page token). A `Watch` event refetches only the first page and splices rows by commit id (GitLens's ledger-diff idea, without the whole ledger). The minimap uses `LogResponse.count` and ref positions only. Keyboard: arrow keys, parent and child, next fork or ref, `/` to search.
- **Smartlog interactions:** single click selects (files of the change, no checkout); double click checks out (Run `switch`); drag reparents; buttons are disabled with a tooltip that says why ("Uncommitted changes: create or stash them first", "Frozen", "Landed").

### 3.6 Conflicts

1. `Status` reports conflicted paths and `in_progress`. The SCM "Conflicts" group and a status-bar banner ("Restack paused: 2 conflicts · Continue · Abort") appear.
2. Opening a conflict calls `Workspace.Conflicts` for `{base, ours, theirs, marked}`. If `commands.getCommands()` includes `_open.mergeEditor`, it is called with `asc:` inputs (`file=<id>`), as the built-in git does (inputs swapped during a restack). `_open.mergeEditor` is an internal, unversioned command, so it is used only when present.
3. Otherwise the working file already holds the diff3 markers (`marked`). VS Code's built-in `merge-conflict` extension, which does not depend on git, adds its Accept current, incoming or both CodeLens.
4. Saving a file with no markers left offers "Mark resolved" → `Workspace.Stage{add}`. "Continue" runs `asc continue`.

### 3.7 Review, inbox, Actions, affected, search

- **Review:** opening a PR (from the inbox, smartlog or a URI) opens the `asc-ui` review panel: the stack navigator, change list, version picker, interdiff and approve/merge. Files open in native diff editors over `asc:` URIs, or all at once with `vscode.changes`. A `CommentController` (`asc.review`) shows Forgejo review threads on those diff editors. New comments go into a pending review submitted from the panel. Line mapping uses the PR's `merge_base` and head (stacks D8).
- **Inbox:** the tree (§3.4) plus a status-bar count of "needs your review". Pin and snooze go through the inbox API.
- **Actions:** runs, jobs and steps per change (Actions context `asc_change`, C40). A log opens as a read-only `asc-actions:` document filled by range reads (tail 64 KiB, "load earlier"). "Re-run" calls the Forgejo API.
- **Affected:** the tree lists projects with their reasons. "Run target" copies `asc affected --target <t> --json` output into a task. No task runner is built in.
- **Search:** "asc: Search code" is a quick pick that streams `CodeIntel.Search` (workspace scope over IPC), cancels on each keystroke and stops at 200 hits. "Open in panel" shows `asc-ui` search with modes (exact, regex, symbol, semantic), a scope picker (workspace, revision, change, stack) and the index coverage from `IndexStatus`. When the server does not advertise `search.v1`, exact search falls back to `Repo.Grep` and the other modes are hidden with the reason.

### 3.8 MCP registration

- `contributes.mcpServerDefinitionProviders: [{ id: "asc", label: "asc" }]`. The provider returns `McpStdioServerDefinition("asc", ascPath, ["mcp"], { cwd: root })` per asc root and `McpHttpServerDefinition("forgejo", <forge>/api/mcp)`. `resolveMcpServerDefinition` adds the bearer token from the auth session.
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

### 3.10 Performance budgets (enforced by P44)

| Metric | Budget | Measured by |
|---|---|---|
| `activate()` in an asc workspace | ≤ 30 ms, no awaited I/O except the marker read | test-electron timing |
| Cost in a non-asc workspace | ≤ 5 ms, no bridge spawned | test-electron |
| First SCM status visible | ≤ 300 ms after activation (ASC, warm daemon) | test-electron with the fixture |
| Extension-host heap attributable to `asc.asc` at idle | ≤ 25 MiB | heap snapshot delta in the test |
| Bridge RSS at idle | ≤ 20 MiB | process stats |
| Webviews at idle | at most one (the smartlog view, only when visible) | test |
| Graph | first 200 rows painted ≤ 150 ms after data; scrolling 100,000 rows ≥ 55 fps; ≤ 5,000 rows in memory | Playwright trace (§6) |
| Event handling | `Watch` events coalesced over 50 ms; ≤ 1 `Status` per 100 ms per root | unit test with a fake clock |
| Bundles | host ≤ 350 KB min; `asc-ui` initial ≤ 300 KB gzip; each lazy view ≤ 150 KB gzip | CI size check |
| Never | `*` activation, `createFileSystemWatcher("**")`, `workspace.findFiles` over the repo, unpaged history, polling without a visible view | oxlint rule + review |

---

## 4. JetBrains plugin

One IntelliJ Platform plugin for IntelliJ IDEA, Rider, RustRover, PyCharm and WebStorm. Kotlin, IntelliJ Platform Gradle Plugin 2.x, `intellijIdea("2026.1")` (Java 21), `sinceBuild = 261`, no `untilBuild`. Depends on `com.intellij.modules.vcs` and `com.intellij.modules.jcef` (required from 2026.2). Split mode and remote development are not supported in v1; the code is kept in `vcs/` (backend-side) and `ui/` (frontend-side) packages so a later split stays mechanical.

### 4.1 Components

| Class | Platform API | asc call |
|---|---|---|
| `AscBridgeService` (application service) | coroutines, `Disposable` | one `asc api --stdio` per IDE process; protobuf-java messages; `Flow` for streams |
| `AscVcs` | `AbstractVcs` (name `asc`), `<vcs name="asc" vcsClass="asc.asc.vcs.AscVcs"/>` | – |
| `AscRootChecker` | `vcsRootChecker` | `.git` gitfile + `asc-workspace` marker |
| `AscMappingActivity` | `ProjectActivity` | maps asc roots to `asc` in `vcs.xml` and drops `Git` mappings for them; one notification |
| `AscChangeProvider` | `ChangeProvider` (background, dirty scopes) | `Workspace.Status(paths of the dirty scope)` → `ChangelistBuilder` |
| `AscWatchService` (project service) | `VcsDirtyScopeManager.filesDirty`, message bus topic `ASC_STATE` | `Workspace.Watch` |
| `AscContentRevision`, `AscDiffProvider` | `ContentRevision`, `DiffProvider`, `RevisionSelector` | `Workspace.ReadFile` (P02), `Log` |
| `AscCheckinEnvironment` | `CheckinEnvironment` + `AmendCommitAware`; `vcs.changes.localCommitExecutor` "Create and submit" | `Workspace.Stage` of the selected changes, then Run `create --no-branch` / `modify` / `submit` |
| `AscRollbackEnvironment` | `RollbackEnvironment` | Run `restore --staged --worktree` |
| `AscAnnotationProvider` | `AnnotationProvider` → `FileAnnotation` (revision, author, date, change, PR in the tooltip) | `Workspace.Blame` (with the unsaved buffer) |
| `AscHistoryProvider` | `VcsHistoryProvider.reportAppendableHistory` (pages of 50); block history | `Workspace.Log{paths, follow}`; `LineHistory` |
| `AscMergeProvider` | `MergeProvider.loadRevisions` → `MergeData`; `conflictResolvedForFile` | `Workspace.Conflicts`, `ReadFile`, `Stage` |
| `AscCheckoutProvider` | `checkoutProvider` (Get from VCS) | Run `clone` |
| `AscToolWindowFactory` | tool window `asc` (JCEF, created on first open) | `asc-ui` tabs: Stack, Graph, Review, Inbox, Actions, Affected, Worktrees, Search, History |
| `AscStatusBarWidgetFactory` | `statusBarWidgetFactory` | change id, stack position, cloud state, "needs restack" |
| `AscActions` | action group in VCS menu and Commit toolbar | up, down, restack, sync, submit, absorb, undo, redo |
| `AscConfigurable` | `projectConfigurable` under Version Control | `asc` path, inline blame, forge account |

Not registered in v1: `logProvider` (ID11), `pushSupport` (asc has no separate push step for drafts; `submit` is an action), `UpdateEnvironment` (sync is an action).

### 4.2 JCEF host for `asc-ui`

- Check `JBCefApp.isSupported()`. Without JCEF, the tool window shows a notice and a link to the same view on the Forgejo fork (§4.3 islands).
- `JBCefBrowser` loads `https://asc-ui.local/index.html`. A `CefRequestHandler`/`CefResourceRequestHandler` serves the bundle from jar resources (the `JCefImageViewer` pattern), so nothing is loaded from the network.
- **Relay:** `JBCefJSQuery` carries base64 bridge frames JS → Kotlin, and `executeJavaScript("window.ascHost.deliver(...)")` carries them back. The allowlist and confirmation rules of §3.5 apply unchanged (same table, generated into Kotlin from `packages/asc-client-ts/src/transport/allowlist.json`).
- **Theme:** Kotlin computes the `--asc-*` tokens from `JBColor`/`UIManager` on open and on `LafManagerListener`, and pushes them as a `theme` message. `JBCefScrollbarsHelper` styles the scrollbars.
- **Lifecycle:** the browser is created on first open and disposed when the tool window has been hidden for 10 minutes. `asc-ui` restores its state through `Platform.getPersistedState` (`PropertiesComponent`).
- **Host actions** (`Platform.openFile`, `openDiff`, `confirm`) map to `FileEditorManager`, `DiffManager` with `AscContentRevision`s, and `Messages`.

### 4.3 Forgejo fork reuse

`asc-ui` is built a second time as a self-contained ESM bundle and vendored into the fork at `public/assets/asc-ui/<version>/` (P07). A Go template partial mounts an island: `<div data-asc-ui="graph" data-repo="owner/repo">`. In the browser, the `Platform` uses the `grpcWeb` transport to a same-origin, read-only proxy `/-/asc/rpc/*` (P04). The proxy checks Forgejo's repo permission, then forwards with the internal token. Writes from the web go through Forgejo's own handlers (merge, restack button). Suggested adopters: the commit graph page (C42) and visual file history; the existing Go templates stay valid.

---

## 5. Auth, offline and local-only

| Topic | Decision |
|---|---|
| Credential store | asc's, and only asc's: `ASC_TOKEN`, else `git credential fill` for the forge host (SPEC §15.1). The plugins never store tokens themselves. `Bridge.Token` reads, `Bridge.StoreToken` writes (`git credential approve`), so the CLI, `asc mcp`, VS Code and JetBrains share one sign-in. |
| asc server calls | Authenticated inside the bridge. The plugin never sees the asc server token path. |
| Forgejo REST calls | Bearer token from `Bridge.Token`, attached by the host (extension host or Kotlin). Never relayed to a webview. |
| VS Code sign-in | `AuthenticationProvider` `asc-forgejo`. `getSessions` → `Bridge.Token`. `createSession` → Forgejo OAuth2 authorization code with PKCE for the pre-registered public client `asc-ide` (P06). The redirect is `${env.uriScheme}://asc.asc/auth` through `env.asExternalUri` + `onUri`, so VS Code, Cursor and VSCodium all work. The token is then saved with `Bridge.StoreToken`. A PAT can be pasted instead. |
| JetBrains sign-in | The same OAuth flow with a loopback redirect on the IDE built-in server (a `RestService` on `127.0.0.1:63342`, as the bundled GitHub plugin does), or a pasted PAT. |
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
| Bridge (Rust) | `cargo test -p asc --test bridge` | framing, routing, cancel kills `Run` children, `Token` never leaves stdout unrequested, no-daemon path | PR |
| `@asc/asc-client` | `bun test` | transports against a real `asc api --stdio` on a fixture; the relay allowlist; Forgejo client against the mock | PR |
| `@asc/asc-ui` logic | `bun test` | preview appliers equal the real result on fixture DAGs (apply verb, re-read `Stacks`, compare); ops queue ordering; graph ring buffer | PR |
| `@asc/asc-ui` UI | Playwright (Chromium) against `asc-ui-devhost` | each view's flows; axe accessibility; screenshots in light, dark and high contrast; keyboard-only navigation; graph performance trace (100,000 rows) | PR |
| VS Code | `@vscode/test-cli` + `@vscode/test-electron` (Mocha in the extension host) on the bundled extension, VS Code stable and the `engines` minimum, Windows and Linux | SCM groups and counts, decorations, blame text, `asc:` content, commands produce the expected `OpResult`, conflicts flow, relay rejects non-allowlisted methods, activation budgets, inert in a non-asc folder | PR |
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

## 7. Proposed PLAN units (P01–P44)

PLAN conventions apply: small units, disjoint owned files, one acceptance command each. Stack ids are PLAN's C ids (stacks.md's S ids renamed). TypeScript units are done when `bunx turbo run lint typecheck` stays green as well. Rust units also keep `cargo clippy` green; Go units run from `D:\dev\forgejo`; JetBrains commands run from `plugins/jetbrains`.

**Recorded amendments** (hunks in files that other units own):

| Owning unit and file | Hunk | Made by |
|---|---|---|
| U50 `crates/asc/src/cli/api.rs` | the `--stdio` flag dispatching to `crate::bridge` | P01 |
| U73 `proto/asc/v1/workspace.proto`, `docs/plugin-api.md` | `rpc ReadFile(WsReadFileRequest) returns (stream ReadFileResponse)` with `oneof at { rev_path, file_id, stage }`; one doc row | P02 |
| C14 `crates/asc/src/stack/cli/reorder.rs` | `--todo <file>` dispatching to `reorder_todo.rs` | P03 |
| M02 `proto/asc/v1/cli.proto` | `StackTodo { repeated TodoItem items }` (`pick`, `fold`, `drop`, `reword`, ordered bottom to top) | P03 |

| ID | Title | Package | Depends | Owns | Accept |
|---|---|---|---|---|---|
| P01 | `bridge.proto` and `asc api --stdio` (framing, routing via the M09 backend, `Run` with typed `OpResult`, `Token`/`StoreToken`, `Events`) | asc, asc-rpc | U50, U52, M02, M09 | `proto/asc/v1/bridge.proto`, `crates/asc/src/bridge/{mod.rs,frame.rs,route.rs,run.rs,auth.rs}`, `crates/asc/tests/bridge.rs` | `cargo test -p asc --test bridge && buf lint proto` |
| P02 | `Workspace.ReadFile` (rev, file id, stage set; offline from the local store) | asc | U73, C31 | `crates/asc/src/daemon/plugin_api_read.rs`, `crates/asc/tests/plugin_api_read.rs` | `cargo test -p asc --test plugin_api_read` |
| P03 | `asc reorder --todo <file>`: one-op stack edit (pick, fold, drop, reword) | asc | C14, C13, C15, M02 | `crates/asc/src/stack/cli/reorder_todo.rs`, `crates/asc/tests/stack_reorder_todo.rs` | `cargo test -p asc --test stack_reorder_todo` |
| P04 | Forgejo read-only asc RPC proxy `/-/asc/rpc/*` (gRPC-Web in, raw passthrough to asc, method allowlist, repo permission) | forgejo | K06, K07 | `routers/web/repo/asc_rpc_proxy.go`, `routers/web/repo/asc_rpc_proxy_test.go` | `go test ./routers/web/repo/... -run TestAscRpcProxy` |
| P05 | Forgejo Actions REST for plugins: runs and jobs by `asc_change`, ranged and tailed job logs (reuses the M24 services) | forgejo | K14, C40, M24 | `routers/api/v1/repo/action_asc.go`, `routers/api/v1/repo/action_asc_test.go` | `go test ./routers/api/v1/repo/... -run TestAscActionsAPI` |
| P06 | Forgejo OAuth public client `asc-ide` (PKCE; `vscode://`, `cursor://`, `vscodium://` and loopback redirects) | forgejo | M26 | `routers/web/auth/oauth_asc_ide.go`, `routers/web/auth/oauth_asc_ide_test.go` | `go test ./routers/web/auth/... -run TestAscIdeClient` |
| P07 | Forgejo `asc-ui` islands: vendored bundle, mount partial, version pin | forgejo | P04, P17, P18 | `public/assets/asc-ui/**`, `templates/base/asc_ui.tmpl`, `modules/ascui/{version.go,version_test.go}` | `go test ./modules/ascui/...` |
| P08 | Monorepo tooling: Bun workspaces, Turbo pipeline, tsconfig, oxlint, Biome, CI workflow | root | – | `package.json`, `bun.lock`, `turbo.json`, `tsconfig.base.json`, `.oxlintrc.json`, `biome.json`, `.forgejo/workflows/ts.yml` | `bun install --frozen-lockfile && bunx turbo run lint typecheck` |
| P09 | `@asc/asc-client`: codegen and the `bridgeNode` transport (restart, cancel, streams) | asc-client-ts | P08, P01, U41, C17, G09, B07, I12, M02 | `proto/buf.gen.ts.yaml`, `packages/asc-client-ts/{package.json,src/index.ts,src/ids.ts,src/transport/{frame.ts,bridge-node.ts}}`, `packages/asc-client-ts/test/bridge.test.ts` | `bunx turbo run test --filter=@asc/asc-client -- test/bridge.test.ts` |
| P10 | `@asc/asc-client`: `relay`/`host` transports, `allowlist.json`, `grpcWeb` | asc-client-ts | P09 | `packages/asc-client-ts/src/transport/{relay.ts,host.ts,grpc-web.ts,allowlist.json}`, `packages/asc-client-ts/test/relay.test.ts` | `bunx turbo run test --filter=@asc/asc-client -- test/relay.test.ts` |
| P11 | `@asc/asc-client`: Forgejo REST client (types generated from the fork's swagger) | asc-client-ts | P09, C41b, P05 | `packages/asc-client-ts/src/forgejo/**`, `packages/asc-client-ts/scripts/gen-forgejo.ts`, `packages/asc-client-ts/test/forgejo.test.ts` | `bunx turbo run test --filter=@asc/asc-client -- test/forgejo.test.ts` |
| P12 | `@asc/asc-fixtures`: deterministic scenarios (§6.2) | asc-fixtures | P08, C11, C13, C14, C15, C24, B05, U54w | `packages/asc-fixtures/**` | `bunx turbo run test --filter=@asc/asc-fixtures` |
| P13 | `@asc/asc-ui` core: `Platform`, theme tokens, state, ops queue, preview appliers | asc-ui | P10 | `packages/asc-ui/{package.json,src/platform.ts,src/theme/**,src/state/**,src/ops/**}`, `packages/asc-ui/test/ops.test.ts` | `bunx turbo run test --filter=@asc/asc-ui -- test/ops.test.ts` |
| P14 | `@asc/asc-ui-devhost` (Hono) with the mock Forgejo, and the Playwright harness | asc-ui-devhost | P13, P11, P12 | `packages/asc-ui-devhost/**`, `packages/asc-ui/playwright.config.ts`, `packages/asc-ui/e2e/smoke.spec.ts` | `bunx turbo run test:e2e --filter=@asc/asc-ui -- e2e/smoke.spec.ts` |
| P15 | `asc-ui` smartlog: stack rows, uncommitted node, drag reparent, cloud drafts, hide, disabled reasons | asc-ui | P14, C08, C49, B15, B16 | `packages/asc-ui/src/views/smartlog/**`, `packages/asc-ui/e2e/smartlog.spec.ts` | `bunx turbo run test:e2e --filter=@asc/asc-ui -- e2e/smartlog.spec.ts` |
| P16 | `asc-ui` stack editor (reorder, fold, drop, split, reword in one op) | asc-ui | P15, P03, C13 | `packages/asc-ui/src/views/stack-editor/**`, `packages/asc-ui/e2e/stack-editor.spec.ts` | `bunx turbo run test:e2e --filter=@asc/asc-ui -- e2e/stack-editor.spec.ts` |
| P17 | `asc-ui` commit graph (virtualised, canvas lanes, minimap, filters, keyboard) with the 100,000-row performance test | asc-ui | P14, C29, C30c, C31 | `packages/asc-ui/src/views/graph/**`, `packages/asc-ui/e2e/graph.spec.ts`, `packages/asc-ui/e2e/graph-perf.spec.ts` | `bunx turbo run test:e2e --filter=@asc/asc-ui -- e2e/graph.spec.ts e2e/graph-perf.spec.ts` |
| P18 | `asc-ui` commit details and compare | asc-ui | P17, C30b, C19 | `packages/asc-ui/src/views/details/**`, `packages/asc-ui/src/views/compare/**`, `packages/asc-ui/e2e/details.spec.ts` | `bunx turbo run test:e2e --filter=@asc/asc-ui -- e2e/details.spec.ts` |
| P19 | `asc-ui` review panel (navigator, versions, interdiff, badges, comment list, approve, merge) | asc-ui | P14, P11, C20, C21, C50 | `packages/asc-ui/src/views/review/**`, `packages/asc-ui/e2e/review.spec.ts` | `bunx turbo run test:e2e --filter=@asc/asc-ui -- e2e/review.spec.ts` |
| P20 | `asc-ui` inbox (sections, pin, snooze) | asc-ui | P14, P11, C41b | `packages/asc-ui/src/views/inbox/**`, `packages/asc-ui/e2e/inbox.spec.ts` | `bunx turbo run test:e2e --filter=@asc/asc-ui -- e2e/inbox.spec.ts` |
| P21 | `asc-ui` Actions runs and logs, affected list, project graph | asc-ui | P14, P11, P05, G09 | `packages/asc-ui/src/views/{actions,affected,projects}/**`, `packages/asc-ui/e2e/ci.spec.ts` | `bunx turbo run test:e2e --filter=@asc/asc-ui -- e2e/ci.spec.ts` |
| P22 | `asc-ui` visual file history and worktrees tab | asc-ui | P14, C31 | `packages/asc-ui/src/views/{history,worktrees}/**`, `packages/asc-ui/e2e/history.spec.ts` | `bunx turbo run test:e2e --filter=@asc/asc-ui -- e2e/history.spec.ts` |
| P23 | `asc-ui` search (exact, regex, symbol, semantic; scope picker; coverage) | asc-ui | P14, I16, I18 | `packages/asc-ui/src/views/search/**`, `packages/asc-ui/e2e/search.spec.ts` | `bunx turbo run test:e2e --filter=@asc/asc-ui -- e2e/search.spec.ts` |
| P24 | VS Code skeleton: manifest, detection, activation, bridge lifecycle, `git.enabled` prompt, Bun bundle, test harness, activation budgets | vscode | P09, P12 | `plugins/vscode/{package.json,scripts/bundle.ts,.vscode-test.mjs,src/extension.ts,src/bridge.ts,src/repo.ts}`, `plugins/vscode/test/activation.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep activation` |
| P25 | VS Code SCM provider, `asc:` file system, quick diff, file decorations | vscode | P24, P02 | `plugins/vscode/src/{scm/provider.ts,fs/ascfs.ts,decorations.ts}`, `plugins/vscode/test/scm.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep scm` |
| P26 | VS Code create, amend, reword, absorb, stage ranges, discard, status bar, undo/redo | vscode | P25, C11, C12, C15 | `plugins/vscode/src/scm/{commands.ts,stage-ranges.ts,statusbar.ts}`, `plugins/vscode/test/commit.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep commit` |
| P27 | VS Code inline blame, hover, CodeLens, heatmap | vscode | P24, C31, C30d | `plugins/vscode/src/blame/**`, `plugins/vscode/test/blame.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep blame` |
| P28 | VS Code webview hosts for `asc-ui` (smartlog view, panels, serializers), relay, CSP, confirmations | vscode | P24, P10, P15, P17 | `plugins/vscode/src/webview/**`, `plugins/vscode/test/webview.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep webview` |
| P29 | VS Code trees: Worktrees, Op log, File History (file and line mode), revision navigation | vscode | P25, C15, C30a, C31 | `plugins/vscode/src/views/{worktrees.ts,oplog.ts,file-history.ts}`, `plugins/vscode/src/revnav.ts`, `plugins/vscode/test/views.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep views` |
| P30 | VS Code conflicts (merge editor or markers, mark resolved, continue, abort) | vscode | P25, C10, C31 | `plugins/vscode/src/conflicts.ts`, `plugins/vscode/test/conflicts.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep conflicts` |
| P31 | VS Code review: comment controller on `asc:` diffs, stack checkout, Inbox tree and status-bar count | vscode | P28, P19, P20, C26 | `plugins/vscode/src/review/**`, `plugins/vscode/src/views/inbox.ts`, `plugins/vscode/test/review.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep review` |
| P32 | VS Code Actions and Affected trees, `asc-actions:` log documents | vscode | P28, P21, G08 | `plugins/vscode/src/views/{actions.ts,affected.ts}`, `plugins/vscode/src/fs/actions-log.ts`, `plugins/vscode/test/ci.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep ci` |
| P33 | VS Code auth provider and MCP registration (VS Code and Cursor) | vscode | P24, P06, M09, M20 | `plugins/vscode/src/{auth.ts,mcp.ts}`, `plugins/vscode/test/auth-mcp.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep auth-mcp` |
| P34 | VS Code search quick pick and search panel | vscode | P28, P23 | `plugins/vscode/src/search.ts`, `plugins/vscode/test/search.test.ts` | `bunx turbo run test --filter=./plugins/vscode -- --grep search` |
| P35 | VS Code packaging and publishing: `vsce` and `ovsx` from Forgejo Actions; VSIX smoke test on VSCodium | vscode | P24 | `plugins/vscode/{README.md,CHANGELOG.md,icon.png}`, `.forgejo/workflows/vscode-release.yml` | `bunx turbo run package --filter=./plugins/vscode` |
| P36 | JetBrains skeleton: Gradle, protobuf-java from `proto/`, `AscBridgeService`, Turbo wrapper, `verifyPlugin` | jetbrains | P08, P01 | `plugins/jetbrains/{build.gradle.kts,settings.gradle.kts,gradle.properties,package.json}`, `plugins/jetbrains/src/main/resources/META-INF/plugin.xml`, `plugins/jetbrains/src/main/kotlin/asc/asc/bridge/**`, `plugins/jetbrains/src/test/kotlin/asc/asc/bridge/**` | `./gradlew test --tests 'asc.asc.bridge.*' verifyPlugin` |
| P37 | JetBrains `AscVcs`, root checker, mapping activity, `ChangeProvider`, `Watch` → dirty scopes, content revisions, `DiffProvider` | jetbrains | P36, P02 | `plugins/jetbrains/src/main/kotlin/asc/asc/vcs/{AscVcs,AscRootChecker,AscMappingActivity,AscChangeProvider,AscWatchService,AscContentRevision,AscDiffProvider}.kt`, `plugins/jetbrains/src/test/kotlin/asc/asc/vcs/ChangesTest.kt` | `./gradlew test --tests 'asc.asc.vcs.ChangesTest'` |
| P38 | JetBrains checkin (create, amend, create and submit) and rollback | jetbrains | P37, C11, C24 | `plugins/jetbrains/src/main/kotlin/asc/asc/vcs/{AscCheckinEnvironment,AscRollbackEnvironment,AscSubmitExecutor}.kt`, `plugins/jetbrains/src/test/kotlin/asc/asc/vcs/CheckinTest.kt` | `./gradlew test --tests 'asc.asc.vcs.CheckinTest'` |
| P39 | JetBrains annotate, file history, history for selection | jetbrains | P37, C30a, C31 | `plugins/jetbrains/src/main/kotlin/asc/asc/vcs/{AscAnnotationProvider,AscHistoryProvider}.kt`, `plugins/jetbrains/src/test/kotlin/asc/asc/vcs/HistoryTest.kt` | `./gradlew test --tests 'asc.asc.vcs.HistoryTest'` |
| P40 | JetBrains merge provider | jetbrains | P37, C31 | `plugins/jetbrains/src/main/kotlin/asc/asc/vcs/AscMergeProvider.kt`, `plugins/jetbrains/src/test/kotlin/asc/asc/vcs/MergeTest.kt` | `./gradlew test --tests 'asc.asc.vcs.MergeTest'` |
| P41 | JetBrains JCEF tool window for `asc-ui` (resource handler, relay, theme), status-bar widget, stack actions | jetbrains | P36, P10, P15, P17, P19 | `plugins/jetbrains/src/main/kotlin/asc/asc/ui/**`, `plugins/jetbrains/src/test/kotlin/asc/asc/ui/RelayTest.kt` | `./gradlew test --tests 'asc.asc.ui.*'` |
| P42 | JetBrains sign-in (OAuth loopback, PAT) and "Configure agents" (`asc mcp install`) | jetbrains | P36, P06, M09 | `plugins/jetbrains/src/main/kotlin/asc/asc/auth/**`, `plugins/jetbrains/src/test/kotlin/asc/asc/auth/AuthTest.kt` | `./gradlew test --tests 'asc.asc.auth.*'` |
| P43 | JetBrains UI tests (IDE Starter + Driver) and Marketplace publishing | jetbrains | P38, P39, P40, P41 | `plugins/jetbrains/src/uiTest/**`, `.forgejo/workflows/jetbrains-release.yml` | `./gradlew uiTest` |
| P44 | Plugin budget gate: activation time, heap, bridge RSS, bundle sizes, graph fps; fails on regression against `bench/plugins/baseline.json` | bench, CI | P24, P17, P41 | `bench/plugins/{collect.ts,baseline.json}`, `.forgejo/workflows/plugin-budgets.yml` | `bun bench/plugins/collect.ts --check` |

**Order.** P01–P03 follow their Rust dependencies. P08–P14 can start as soon as P01 exists. The first VS Code release is P24–P35 (the asc-ui views it embeds come first). JetBrains (P36–P43) starts after P15, P17 and P19 exist. The Forgejo units P04–P07 run on the Go track after K07, K14 and M26.

---

## 8. Risks and spikes

| Risk | Mitigation |
|---|---|
| `Bun.build` CJS output is experimental | the test-electron suite runs on the bundle; `scripts/bundle.ts` swaps to esbuild in one file |
| `_open.mergeEditor` is internal and unversioned | feature-detected; the marker fallback is fully functional |
| Cursor and VSCodium lag the VS Code engine | spike in P35: install the VSIX on the current Cursor and VSCodium; lower `engines` only if a stable API used here is missing |
| JCEF unavailable (some Linux setups, remote IDEs) | a notice and the Forgejo island fallback (§4.2) |
| Bridge process crash | restart with backoff, `Watch` resumes from its token; the UI shows "reconnecting" |
| Kotlin and TypeScript disagree on the relay allowlist | one `allowlist.json`, generated into Kotlin at build time |

---

## 9. Edits needed in SPEC.md, PLAN.md and other designs (not made here)

| File | Edit |
|---|---|
| SPEC §7 | Add `bridge.proto` (the `Bridge` service and framing) and `Workspace.ReadFile` to the plugin API; note that ASC's plugins reach every RPC through `asc api --stdio`. |
| SPEC §11.2 | `asc api --stdio`; `asc reorder --todo <file>`. |
| SPEC §13.3 | Fork units P04–P07 (RPC proxy, Actions REST, `asc-ide` OAuth client, `asc-ui` islands). |
| SPEC §16.4 | The plugin budgets of §3.10 and the P44 gate. |
| SPEC §17 | The plugin test layers of §6. |
| PLAN | A new "§P IDE plugins track" with P01–P44, the recorded amendments of §7 and the TypeScript rules (Bun, Turbo, oxlint, Biome, strict tsconfig). The workspace layout block gains `packages/` and `plugins/`. |
| stacks.md fixed rules | Replace "there is no IDE extension, so GitLens/GitKraken-style features are data APIs" with "ASC's plugins render these data APIs (`ide-plugins.md`)". |
