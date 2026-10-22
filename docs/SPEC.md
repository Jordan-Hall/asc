# asc: final specification

Status: **authoritative**, v1.8 (ASC-built IDE plugins folded in as §24; v1.7 folded agentic code intelligence in as §23; v1.6 folded built-in MCP in as §22; v1.5 folded branchless by default and commit cloud in as §21; v1.4 closed the round-4 contract gaps: sinks, ref chokepoint, staging, registration, proto, store tiers), 2026-10-22. This document supersedes `docs/designs/*.md`. Where it is silent, the design documents are background only; they are not normative.

Inputs: `docs/designs/{simplicity,integration,performance}.md`, `docs/research/{asc-profile,forgejo,prior-art,rust-stack,turbo-ci,scion-article,stacks,branchless,mcp,codeintel,ide-plugins}.md`, `docs/research/scion-checklist.md` (the required coverage list, including the user requirements), and the three judge reports. Base design: *simplicity* (two of three judges ranked it first). Grafts are marked **[graft: source]**. Fixes for judge-reported flaws are marked **[fix Jn]**, and fixes from the round-1, round-2 and round-3 audits are marked **[audit: gap-id]**. All are listed in Appendix A.

**User decisions applied in v1.1 (scion-checklist.md):** (1) the default branch is `master`, never `main`; (2) git-identical CLI, and asc can be installed as `git`; (3) **no reliance on the IDEs' built-in git** (the IDEs' built-in git is not supported; ASC builds its own VS Code and JetBrains plugins, item (10)): there is no full `.git` projection, git's index is not the staging area, commits made by real git are not imported, and there is no `git-remote-asc` helper. Each workspace keeps a minimal synthetic `.git` for Turborepo only (§9.4), and ASC's IDE plugins use asc's API (§7, §9.1, §24); (4) a built-in polyglot project graph and `asc affected` (§12A); (5) Scion-core memory and storage efficiency is a v1 requirement (§16.4); (6) later checklist decisions: `.ascignore` first (§2.4.2), an ASC Actions fork (§12.7), a benchmark gate against git with small/medium/HUGE workloads (§16.4), and no LFS support (§2.5); (7) stacked commits and stacked PRs are core: specified in new §20 (decisions D1–D12 of `docs/research/stacks.md`, now normative; PLAN units C01–C52); (8) **branchless by default** (checklist decision of 2026-10-22, overriding the earlier "nothing is uploaded before `asc push`", "no write-behind" and "secrets in drafts never leave the machine" rules): shared refs are only `master`, release branches and tags; all other work is draft changes indexed by change id; a pushed branch becomes the pusher's user ref and submits its stack as draft changes; the daemon uploads new commits at once (the opt-out commit cloud) behind a pre-upload secret hold, with purge; off switches per repo and per user: specified in new §21 (decisions BD1–BD12 of `docs/research/branchless.md`, now normative; PLAN units B01–B28) [audit: branchless-folded]; (9) **MCP is built in** (checklist decision of 2026-10-22): `asc mcp` (stdio), `asc serve /mcp` (Streamable HTTP) and the Forgejo fork's `/api/mcp`, all generated from the proto schemas, read-only by default and ACL-scoped: specified in new §22 (decisions M-D1–M-D10 of `docs/research/mcp.md`, now normative; PLAN units M01a–M28) [audit: mcp-folded]. (10) **ASC-built IDE plugins** (checklist decision of 2026-10-22, "as good as GitKraken plus Graphite"): a VS Code extension (Cursor and VSCodium through Open VSX) first, then one IntelliJ Platform plugin; they replace the built-in git SCM in asc workspaces, share one generated TypeScript client and one React UI (`asc-ui`) with the Forgejo fork, and reach asc only through the `asc api --stdio` bridge: specified in new §24 (decisions IP-D1–IP-D15 of `docs/research/ide-plugins.md`, now normative; PLAN units P01a–P58) [audit: ide-plugins-folded].

Conventions: MUST/MUST NOT are requirements. "ASC" means the `asc/dds` monorepo at `D:\dev\monorepo` and its Forgejo at `https://forge.example.org`. Sizes are binary (KiB, MiB, GiB).

---

## 0. The design on one page

**asc** is a source control system modelled on Scion: Bonsai-style changesets, content-addressed immutable objects in an S3-compatible or local object store, refs as compare-and-swap rows, derived data computed from parents, a server that does history, diffs, merges and landing, and clients that fetch only what they touch.

**Deployables**

| Deployable | What it is |
|---|---|
| `asc` | One Rust multi-call binary: CLI, `asc daemon`, `asc serve` (`--role primary`, `--role cache` or `--role indexer`, §23.10), `asc mcp` (§22), and, by argv[0], `git` (the shim, §11.4). Cargo features `serve`, `vfs-projfs`, `vfs-fuse` trim builds. |
| Forgejo fork | Forgejo v16.0.6 plus an ordered patch stack (seams, asc client, hooks, bridge). Go, separate repo, GPLv3. Talks to `asc serve` only over gRPC. |
| `asc/actions` | Forked `checkout`, `cache`, `upload-artifact`, `download-artifact` (aliased from `actions/*`, §12.7) plus `asc/setup`, `asc/changed`, `asc/affected`, `asc/turbo-cache`, all calling `asc ci …` |
| `asc-embed` | The opt-in model helper (fastembed over ONNX Runtime), built for the server and downloaded by `asc codeintel enable-embed` for local semantic search; `asc.exe` never links ONNX Runtime (§23.10) |

**Fifteen decisions**

1. **A remote is a store or a server.** `Objects` (immutable, content-addressed) and `Refs` (CAS). One `Engine` runs over the machine store, an S3/local bucket, or a gRPC client. Local-only, serverless-bucket, git-remote and client/server modes share one code path.
2. **Identity is asc's.** BLAKE3 ids, typed per kind. Git SHA-1s are deterministic derived data. Import is lossless and canonical, so a commit exported from asc and re-imported has the same changeset id **[fix: determinism across modes]**.
3. **The changeset hashes its change list and its root tree.** Validated as `apply(parents[0].root, changes) == root` and `changes == leafdiff(parents[0].root, root)`. The v1 tree format has no size limit and is frozen. A tree-format change would be a history rewrite and is out of scope; this trade is accepted and documented **[fix J3: identity coupling]**.
4. **Tree entries carry the git blob/tree SHA-1 and size.** Export, the synthetic turbo index, turbo hashing and staged-vs-HEAD are metadata-only. Servers recompute and verify every client-supplied git field **[graft: integration] [fix J1]**.
5. **The machine cache is a directory.** Atomic rename, content-addressed names, BLAKE3 verified on every read **[graft: integration]**. `store/` (never evicted by LRU: not-yet-uploaded and local-only data; only the hidden-draft expiry of §21.2.4 removes from it) is separate from `cache/` (evictable copies of remote objects).
6. **asc is git at the surface.** Same commands, flags, output, messages and exit codes as git 2.55 (§11). The `asc` binary installed as `git` serves every git command in an asc workspace natively and falls through to real git everywhere else (§11.4). Each working copy has a **minimal synthetic `.git` for Turborepo only**: `HEAD` plus, when the repo uses turbo or the workspace is plain, an index rendered from tree metadata (§9.4). asc keeps its own staging area (§9.5). Every child asc spawns (hooks, aliases, editors, drivers) finds asc first on `PATH`, so `git` inside a hook is asc (§9.4.1). Real git never operates on an asc workspace, and the IDEs' built-in git is not supported: ASC's own IDE plugins replace it and use asc's API (§24) **[audit: user decision No-IDE, superseded in part by ide-plugins-folded: plugins are built, git plumbing still is not]**.
7. **Lazy materialisation.** ProjFS (Windows) and FUSE (Linux) virtual working copies; plain mode (reflink/copy) everywhere else and as the CI default. Switch updates placeholders from metadata and downloads nothing. Placeholder timestamps are per-placeholder switch-epoch times that only move forward **[fix J1/J2/J3]**.
8. **Server-side history, API-first CI.** `Diff`, `MergeBase`, `Log`, `Blame`, `Affected` run on the server with no checkout. CI plan jobs need one RPC.
9. **Staged, reversible Forgejo cutover.** Stage 0 git-remote mode (no server) → Stage 1 mirror mode (no fork) → Stage 2 asc is truth, Forgejo reads a git bridge → Stage 3 Forgejo reads through asc (Seam A) → Stage 4 Forgejo writes through asc (Seam B, full replacement). `storage_backend` and `object_format_name` are independent switches; git SHAs stay visible until a separate, optional ID flip **[graft: integration D2/D9]**.
10. **One SQL transaction moves refs.** Ref CAS, ref log, idempotency record and outbox row (bridge promotion, post-receive) commit together; the outbox delivers at least once **[graft: integration] [fix J1]**.
11. **Built-in project graph.** asc detects Cargo, Python (uv/pyproject/requirements), .NET (sln/slnx/csproj/fsproj) and Bun/Node workspaces itself, by parsing manifest and lockfile bytes. The graph is derived data per changeset. `asc affected` and the `Affected` RPC map a diff to the owning projects plus their reverse-dependency closure, with no checkout and no hydration. turbo and nx are optional consumers, never dependencies (§12A) **[audit: projgraph]**.
12. **Scion-core efficiency is v1.** Want-only tree-then-blob fetch of stored bytes; a bounded local history window, with older history answered by the server (§6.3); tiers that speak the same RPCs (daemon cache → `asc serve --role cache` → primary); seed snapshots; generation numbers plus skip lists; explicit memory and storage budgets enforced by a benchmark gate in CI (§16.4) **[audit: user decision efficiency]**.
13. **Branchless by default, with a commit cloud.** Shared refs are only `master`, `release/*`, `green/*` and tags (per-repo `shared_refs`). A draft is a non-immutable changeset with a `ChangeMeta`, named by change id; it needs no branch. Visibility is a set of draft heads (`draft_heads` locally, `cloud_heads` per user workspace on the server), not refs. A local branch is a real local ref and a label; a pushed non-shared branch is the pusher's user ref (`refs/users/<owner-id>/heads/<n>`, one CAS row, owner-only), and `git push origin feat` also submits the stack as draft PRs. While online, the daemon uploads new changesets asynchronously (want-only, idempotent) after a pre-upload ignore and secret hold, never on public repos, with owner purge and cache eviction. No local git command changes its output with the mode. Off switches: client `asc.branchless`, server `repos.branchless` and `repos.cloud` (§21) [audit: branchless-folded].
14. **MCP is built in.** Three surfaces, one owner per fact: `asc mcp` (stdio: workspace and repo), `asc serve` `/mcp` (Streamable HTTP 2026-07-28: the repo without a checkout) and the Forgejo fork's `/api/mcp` (PRs, reviews, issues, Actions, inbox). Tool schemas are generated from the proto descriptors; CLI `--json` and MCP share one codec, the asc JSON profile `json.profile.v1`, while `asc api` keeps standard proto3 JSON. Calls are stateless and handle-based. The default is read-only and only the user's own config widens it; destructive actions are confirmed through MRTR elicitation; every result is bounded by a token budget; the SDK is rmcp 3.6.0. See §22 [audit: mcp-folded].
15. **Agentic code intelligence is built in.** Exact (trigram) search, tree-sitter symbols and local-model vector search run on `asc serve` as derived data keyed by `FileId`, so an agent can query any revision, draft or stack with no checkout. Exact search and symbols come first; vectors are the third tool, fused by RRF. One engine (tantivy) holds the postings and BM25; embeddings run on the server CPU by default and no code leaves ASC. Indexes are per namespace and every result is read-ACL filtered inside the ANN traversal. The tools are `search` and `read_file` (§22), and Forgejo code search reads the same service. See §23 [audit: codeintel-folded].
16. **ASC builds its own IDE plugins.** VS Code (Cursor and VSCodium through Open VSX) first, then one IntelliJ Platform plugin. They replace the built-in git SCM in asc workspaces (`git.enabled: false`; JetBrains maps the roots to `asc`), render a native SCM view plus one shared React UI (`asc-ui`: smartlog, graph, review, history), and reach asc only through one `asc api --stdio` bridge, so neither Node nor the JVM touches the daemon pipe and no plugin parses human output. One generated TypeScript client and `asc-ui` also serve the Forgejo fork. Local features never need an account. See §24 [audit: ide-plugins-folded].

**Not built in v1** (each is additive):
- *Measured v1.x items:* segment packing of small objects, zstd dictionaries, logical dedup across a fork network (§3.3; deviation recorded as §19 decision 6).
- *Gated on measurement:* a path-log table beyond the mainline index (§5.1), virtual CI, daemon-side turbo cache tier, a host-level fetch proxy for CI.
- *Out of scope:* SSH `asc-authenticate`, jj-style first-class conflicts, workspace sync, macOS NFS/FSKit, `node_modules` snapshots, `WatchRefs`, `asc run` task execution, TypeScript import-graph inference, parsing `package-lock.json`/`pnpm-lock.yaml`/`yarn.lock`/`bun.lockb` (they are eco-global files, §12A.2), compatibility with the IDEs' built-in git (ASC's plugins replace it, §24), and real git operating inside an asc workspace.

---

## 1. Requirements traceability

| Fixed requirement / checklist section | Where met |
|---|---|
| New changeset model, content-addressed; git derived | §2, §5 (`gitcommit`), §14 |
| Full storage replacement in Forgejo | §13 stages 2–4 |
| Rust client, daemon, server | §11, §9, §8 (one binary) |
| `asc` mirrors git commands and flags | §11 |
| S3-compatible and local storage; local-only and client/server | §3, §6 |
| Lazy materialisation on checkout/switch/branch | §10 |
| Diffs server-side or from metadata | §4.3, §7 `Diff` |
| Cheap worktrees, shared object cache; `worktree add` cost independent of repo size | §3.5, §9.3, §9.4 (`asc.turboIndex`), §16.4; `worktree convert --plain` fallback (§9.1, §11.2) |
| Faster Turborepo CI | §12, §12A; §12.1 staged files hash as in git; §12.2 rules read at the merge base without checkout; §12.3/§3.5 tier stack keys |
| **Default branch is `master`, never `main`** | §6.5; CI lint covering push/pull/merge/rebase/`-b`/`--base`/`ref:`/`default_branch`/YAML list forms over docs, README, crates, tests, deploy, actions, bench, packaging, proto and build (§6.5); `-c init.defaultBranch=master` for every real-git script and `bench/gen` (§6.5); fixtures (§17.1); Forgejo `DEFAULT_BRANCH` (§6.5); stack trunk default (§20.3) |
| **TOP: git-identical DX**: same commands, flags, revisions, output, exit codes; byte-compatible `--porcelain`/`-z`/`--format` | §11.1, §11.2, §11.5 (native table), §4.3 diff contract, §11.1 ids; §11.4 verb precedence (git builtins → `alias.*` → `git-<name>` → asc-only verbs, so aliases and external commands keep working); §9.7 staging rollback on hook failure; §9.4 index mirrors staging; §9.4.1 single ref-update chokepoint (hooks, reflog) |
| asc installable as `git`, falls through to real git outside asc repos and for repository-independent commands; IDEs and tools that run `git` work | §11.4 (dispatch, Windows machine-PATH order, target-based routing of `clone`/`init`/`ls-remote`, external-command lookup), §9.4.1 child PATH |
| `git` inside hooks, aliases, editors and drivers reaches asc | §9.4.1 (child PATH) |
| Hook tools work unchanged (husky, lint-staged, commitlint, Claude Code) | §9.4.1, §9.5 (`GIT_INDEX_FILE` stage sets), §9.7 (hook order), §17.1 |
| IDEs' built-in git is not supported (ASC's plugins are): no full projection, no git-index staging, no import of `git commit`, no `git-remote-asc`; turbo-only synthetic `.git`; plugin API (ASC's IDE plugins) [audit: ide-plugins-folded] | §0 decision 6, §9.4, §9.5, §9.6, §7 `workspace.proto` (messages defined), §11.2 `--json`/`asc watch`, §9.1 |
| **ASC-built IDE plugins** (decision 2026-10-22): VS Code (Cursor, VSCodium) first, then JetBrains; they replace the built-in git SCM, with the feature list of GitKraken and Graphite in one place; one generated TypeScript client shared with the Forgejo UI; no parsing of human output; the asc performance budgets | §24 (IP-D1–IP-D15), §0 decision 16, §7 (`bridge.proto`, `Workspace.ReadFile`), §20.6, §11.2, §13.3 (P04–P07, P58), §16.4, §17 |
| Same files and config: `.gitignore`, `.gitattributes`, git config, aliases, hooks, credential helpers, `GIT_*` env | §11.1 (env and global options), §2.4.1, §2.4.2, §9.4.1, §9.4 (`includeIf`) |
| Same mental model: commit then push, push is the CAS, branches are local labels, drafts are uploaded in the background | §6.1 (branch scope), §8.5 (land only on request), §2.7 (ref scope), §21 |
| Familiar errors and hints | §11.6 |
| Unavoidable differences invisible or additive | §11.5 (asc-only flags), §8.5 (`--land` explicit), §2.4.1 (documented attribute deviation), §17.1 `allowed.toml` |
| Compatibility test suite as an acceptance gate | §17.1, §18 exit criteria |
| **TOP: built-in polyglot project graph and `asc affected`** (Rust, Python, .NET, Node/Bun), API-first; `--base` defaults to the default branch | §12A, §5, §7 `Projects`, §11.2, §12.2, §12A.9 defaults |
| Scion principles 1–7, data model 8–10, fetching and tiers 11–20, migration lessons 21–23, quality and operations 24–30, advice 31–32 | §1.1 |
| **TOP: Scion-core memory and storage efficiency** | §6.3 (want-only; history window), §3.2 (stored bytes), §7 (tier contract, cache role), §12.3 (seeds), §5 (`skip.v1`), §5.1, §3.5 (storage budgets), §3.3 (bucket efficiency; logical fork-network dedup deferred, §19 decision 6), §16.4 (memory budgets, `limits.text_memory`, benchmark gate) |
| Ignore files: `.ascignore` first, `.gitignore` fallback; `.ascattributes` first | §2.4.2 (incl. shadow warning, `migrate ignore` copies), §2.4.1 |
| Own Actions fork: CI uses asc, not git | §12.7 (`asc/actions`, cache/artifact store contract), §12.3 step 6, §13.3 (runner fork), §19 decision 5 |
| Benchmark against git: asc must be faster | §16.4 benchmark gate, `docs/BENCHMARKS.md` |
| Checkout speed: small, medium and HUGE monorepos | §16.4 workloads and goal; §6.3 history window (clone O(window + root tree)) |
| No LFS: pointer files are ordinary blobs; LFS content archived outside asc; large files native with no setup | §2.5, §2.4.1 (`filter=lfs` ignored), §14.1, §14.3, §13.3 K13 |
| Stacked commits and stacked PRs are core (decision 2026-10-22) | §20 (stack model D1–D3, op log/undo D6, in-memory restack D7, stacked PRs D8/D11, land D10, GitLens/GitKraken data D12); §11.2 stack verbs; §11.4 D5 precedence; §13.3 C34–C48; §7 and §20.5 `Changes` service; §2.2/§3.1 `ChangeMeta`; §12.6 stack queue; §17.1 invisibility case |
| Stack and review features in the Forgejo fork UI (stack navigator, per-change review, stack merge through the queue, commit graph, blame/file history, inbox, comparison, undo-aware operations) | §20.8, §13.3 C34–C48, §20.5 flows |
| GitLens/GitKraken data features in core and the plugin API (blame, history, graph, worktrees, conflicts) | §20.6, §7 `workspace.proto` additions |
| **Branchless by default** (decision 2026-10-22): shared refs are only `master` and release branches; other work is draft changes indexed by change id and visible at once; `git push origin feat` submits the stack as draft changes; the daemon uploads commits asynchronously; off switch per repo and per user; local-only mode still works | §21 (BD1–BD12), §0 decision 13, §6.1, §6.3, §6.4, §2.7, §3.5, §3.6, §8.2, §8.3, §9.1, §11.1, §11.2, §13.3, §15.2, §16.3, §17.1; PLAN units B01–B28 |
| **MCP built in** (decision 2026-10-22): `asc mcp`, `asc serve /mcp` and Forgejo `/api/mcp`; schemas generated from proto; read-only by default; ACL-scoped; bounded results | §22 (M-D1–M-D10), §0 decision 14, §7 (`options.proto`, `cli.proto`, `/mcp`, the JSON profile), §8.1 `[mcp]`, §8.7, §11.2 `asc mcp`, §13.3 M20–M28, §15.1, §16.3, §16.4, §20.7 (`--json` shapes), §21.7; PLAN units M01a–M28 |
| **Agentic code intelligence built in** (decision 2026-10-22): trigram exact and regex search, tree-sitter symbols for Rust, TypeScript, Python, C# and Go, and local-model vector search with hybrid ranking; server-side derived data keyed by `FileId`, ACL-filtered, incremental per commit; exposed through MCP, RPC, IPC, `--json` and Forgejo search; offline queries from the daemon | §23 (CI-D1–CI-D8), §0 decision 15, §3.3, §5, §7, §8.1, §8.2, §9.1, §11.2, §13.3 I23–I26, §15, §16.1, §16.3, §16.4, §18 (phases 4c, 4c-MCP, 5c, 9c), §22.3.2; PLAN units I01–I31 |
| **ASC-built IDE plugins** (decision 2026-10-22): VS Code (Cursor, VSCodium) first, then JetBrains; they replace the built-in git SCM, with the feature list of GitKraken and Graphite in one place; one generated TypeScript client shared with the Forgejo UI; no parsing of human output; the asc performance budgets | §24 (IP-D1–IP-D15), §0 decision 16, §7 (`bridge.proto`, `Workspace.ReadFile`), §20.6, §11.2, §13.3 (P04–P07, P58), §16.4, §17 |

ASC workload (from `asc-profile.md`): 4,543 files / 119 MiB at HEAD, 5,502 commits, 104,800 reachable objects (311 MiB packed), 113 historical LFS objects (25.6 GiB), one submodule `packages/soma/sdk`, 50 worktrees, 26 commits/day mean (303 peak), median 4 files per commit.

**Targets**

| Operation | Target (ASC, warm machine cache) |
|---|---|
| `asc clone` (virtual) | < 2 s, < 5 MiB downloaded |
| `asc worktree add` | < 1 s, < 1 MiB written before builds |
| `asc status` clean, virtual | < 50 ms |
| `asc switch` between trunk tips | O(changed paths), 0 file downloads |
| CI plan job | < 10 s, no checkout |
| CI plain checkout from warm host cache | < 3 s |
| Turbo task hashes on clean virtual copy | equal to a git clone, 0 hydrations beyond manifests |
| Per-machine footprint replacing 58 GB `.git` | < 2 GiB plus hydrated content |
| `asc affected` / `Affected` RPC | < 300 ms warm, < 2 s cold (§12A.8) |
| Shim fall-through overhead (`git` in a non-asc repo) | p95 ≤ 5 ms Windows, ≤ 2 ms Linux (§11.4) |
| Daemon idle RSS | ≤ 64 MiB (§16.4) |

### 1.1 Scion checklist traceability (items 1–32)

Every item of `scion-checklist.md` is either met or dropped with a reason.

| # | Item | asc handling | Where |
|---|---|---|---|
| 1 | Always online, plus local-only | Online by default; local-only mode; offline subset; history beyond the local window is answered by the server | §6.1, §6.3, §6.4 |
| 2 | Client-server asymmetry | Server does history, blame, merge, land, rebase; client keeps a bounded cache and a bounded history window | §4, §8, §3.5, §6.3 |
| 3 | API-first | Every client query is a public RPC; `asc api` | §7 |
| 4 | Horizontal scale, no packfiles, no maintenance | No packfiles; `gc`/`repack` are no-ops | §3.3, §11.2 |
| 5 | Cheap commits, cost scales with the change | A local commit costs one row; the daemon uploads it in the background (§21.3.1); server `Commit`/`Merge`/`Apply`; ingest O(changed paths × depth) | §8.7, §8.4, §21 |
| 6 | Few global refs | Shared refs are only `master`, release branches and tags; a pushed branch is one owner-only user ref; drafts are heads, not refs; agent refs expire and are not fetched by default; changesets need no ref | §2.7, §6.1, §8.7, §21.1, §21.5.1 |
| 7 | Optional, sparse, virtual working copy | Virtual and sparse workspaces; writes through the API | §10, §8.7 |
| 8 | Changeset is a WAL entry | Changes relative to the first parent; visible on ingest | §2.6, §8.7 |
| 9 | Accepting changesets needs no ref move | `Commit` without `update_ref`; `Ingest` RPC | §8.7 |
| 10 | One generic derived-data mechanism | `Derived` and `DerivedKeyed`; named future types | §5 |
| 11 | Content-addressed, immutable | BLAKE3 ids, create-only puts | §2.1, §3.3 |
| 12 | Want-only negotiation | Explicit wants; Log-driven history fetch; resumable | §6.3 |
| 13 | Sparse/virtual need no protocol support | The client does not ask | §6.3, §10 |
| 14 | Per-node daemon | Per-user daemon plus shared cache; CI host cache as the node store | §9.1 |
| 15 | Daemon boots from a seed | Seed snapshots published by the server | §12.3 |
| 16 | Caches stream stored bytes | `get_stored`; no per-client computation | §3.2 |
| 17 | Every tier speaks the same RPCs | Object tier contract; `asc serve --role cache` | §7 |
| 18 | Strict tiering | Retries with budget, coalescing, shedding, no tier bypass | §16.3 |
| 19 | Stateless tiers, > 99% hit rate | Cache SLOs; every cache is disposable | §16.4 |
| 20 | No client or cache maintenance | No packfiles, no projection ODB, LRU caches | §3.5, §9.4 |
| 21 | Metadata first | Milestone M-meta | §18 |
| 22 | Ingestion invariants, memory and speed | I1–I7 | §14.4 |
| 23 | No brute-force lookups | `land_map`, `FindLanding`, mainline index | §8.9, §5.1 |
| 24 | Hermetic simulation, fuzzing, rate limiting | `asc-sim`, fuzz targets, admission control | §17.2, §17.3, §16.3 |
| 25 | No over-engineering; no re-validation of trusted services | Trust boundaries | §15.4 |
| 26 | Repo ACLs plus directory permissions | Write rules in pre-receive; read isolation by composed repos | §15.5 |
| 27 | Semantic review constraints | `Classify` RPC; Forgejo relaxed approvals (K17) | §8.10 |
| 28 | Post-merge testing; agents resolve conflicts | `Rebase` RPC with structured conflicts; content-merge land; `green/*` refs | §8.11, §12.6 |
| 29 | Per-component monitoring | Per-tier metric and log catalogue; runbooks | §16.1 |
| 30 | Ancestry through indexes | Generation numbers, skip lists, mainline and path index | §4.3, §5, §5.1 |
| 31 | Standardised server RPCs | `asc.v1` public API, forge-neutral; `asc api` | §7 |
| 32 | Negotiation can change; sparse and shallow compose | No asc-side git helper (the full-projection decision, §0 principle 6; ASC's plugins use the asc API, §24). The Forgejo git bridge is served by real git, which handles `filter`/`shallow`/`deepen` | §13.1, §9.6 |

---

## 2. Data model (crate `asc-core`: pure, no I/O, no async)

### 2.1 Identifiers

All ids are 32-byte BLAKE3 outputs using **derive-key mode** with a per-kind context string:

| Type | Context string | Hashed bytes |
|---|---|---|
| `FileId` | `"asc v1 file"` | the raw file content (whole file, any size) |
| `ChunkId` | `"asc v1 chunk"` | the raw chunk bytes |
| `TreeId` | `"asc v1 tree"` | the canonical envelope of a `Tree` (§2.2) |
| `ChangesetId` | `"asc v1 changeset"` | the canonical envelope of a `Changeset` |
| `TagId` | `"asc v1 tag"` | the canonical envelope of a `TagRecord` |

`id = blake3::Hasher::new_derive_key(CONTEXT).update(bytes).finalize()`.

- Each is a distinct Rust newtype over `[u8; 32]` with no shared base type. `GitOid([u8; 20])` is a separate newtype and is never an identity inside asc.
- Display: 64 lowercase hex; abbreviated display of asc ids (opt-in, §11.1) is 12 hex. Input accepts full hex or a unique prefix of ≥ 4 hex characters. A 40-hex string is a git SHA. A prefix that matches both an asc id and a git SHA is an error that lists candidates. Explicit prefixes `asc:` and `git:` force one interpretation.
- The empty tree has a fixed `TreeId` = hash of the envelope of `Tree { entries: [] }`; golden value in `crates/asc-core/tests/golden/ids.txt`.

### 2.2 Canonical encoding

**Envelope:** `b"asc"` (0x63 0x67) ‖ `kind: u8` ‖ `version: u8` ‖ `postcard(payload)`.

| kind | payload | version |
|---|---|---|
| 1 | `Tree` | 1 |
| 2 | `Changeset` | 1 |
| 3 | `TagRecord` | 1 |
| 4 | `ChunkList` | 1 |
| 5 | reserved (was `FileMeta`; dropped from v1, server metadata lives in SQL and derived data) | – |
| 6 | `ChangeMeta` (stable change identity sidecar, §20.2) | 1 |
| 16–63 | derived values (§5) | per type |

Postcard 1.x rules (normative, golden-tested): unsigned ints are LEB128 varints, signed ints are zigzag varints, `u8` is one raw byte, `bool` is one byte 0/1, `Option` is a tag byte 0/1 then the value, `Vec<T>`/`Vec<u8>` is a varint length then elements, `[u8; N]` is N raw bytes with no length, enums are a varint variant index then fields, structs are fields in declaration order. Forbidden in encoded types: `HashMap`, `f32/f64`, `String` (use `Vec<u8>` except where noted), `#[serde(skip_serializing_if)]`, `flatten`, untagged enums. Maps are `Vec<(K, V)>` sorted strictly ascending by key bytes (no duplicates); decoders MUST reject unsorted or duplicate keys, trailing bytes, unknown kinds and unknown versions. Therefore `encode(decode(b)) == b` for every accepted `b`.

### 2.2a Versioned non-envelope encodings [audit: rolling-upgrade-encodings]

Every postcard value stored outside an envelope starts with `version: u8`. This covers `BucketRefEntry` (v1), the commit `asc` header (`asc <base64(0x01 ‖ postcard(AscGitExtra))>`), the seed manifest, and `state.db` blobs. `AscGitExtra` v1 is frozen like the tree format. An importer that sees an unknown `asc` header version fails with `upgrade asc`. It never stores that commit raw, because that would fork ids. A `BucketRefs` reader that sees an unknown entry version fails with `upgrade asc` and never writes.

### 2.3 Names and paths

- `Name`: non-empty `Vec<u8>`; MUST NOT contain `/` or NUL; MUST NOT be `.` or `..`; MUST NOT equal `.git` under ASCII case folding. No other restriction at the data-model level (history may contain anything git allowed).
- `RepoPath`: one or more `Name`s joined by `/`. No leading or trailing `/`. Ordering is by raw bytes.
- `casefold(name)` = Unicode simple case folding of the UTF-8 decode, or ASCII lowercase of raw bytes when not valid UTF-8.
- `windows_valid(name)` is false when the name: ends in `.` or space; contains any of `< > : " \ | ? *` or bytes 0x01–0x1F; or its stem before the first `.` is (case-insensitively) `CON PRN AUX NUL COM1–COM9 LPT1–LPT9`.
- `checkout_safe(path)` applies git's `verify_path` rules with `core.protectNTFS=true` and `core.protectHFS=true` (rejects `.git` variants such as `GIT~1`, `.git::$INDEX_ALLOCATION`, `.git` with HFS-ignorable code points).

Where each rule applies is in §2.8.

### 2.4 File content

- A file's identity is `FileId` over its whole content. Chunking never affects identity.
- **Inline files** (size ≤ 4 MiB = 4,194,304 bytes) are one object.
- **Chunked files** (size > 4 MiB) are a `ChunkList` plus chunks. FastCDC v2020 from `fastcdc` 5.0.0, min 512 KiB, avg 1 MiB, max 4 MiB, `Normalization::Level1`.

```rust
pub struct ChunkList { pub size: u64, pub chunks: Vec<(ChunkId, u32)> } // u32 = chunk length; sum == size
```

`ChunkList` is not identity-bearing: it is stored under the file's `FileId`, and a reader accepts any `ChunkList` whose assembled bytes hash to that `FileId`. A reader verifies each chunk against its `ChunkId` while streaming, and the whole file against `FileId` at the end. On mismatch it discards the result and reports corruption.

- Bytes are stored exactly as committed. Checkout performs no EOL or filter conversion. Ingest applies git's normalisation only as §2.4.1 says.

#### 2.4.1 Attributes [audit: gitattributes, gitattributes-normalisation]

- asc reads attributes with `gix-attributes` from, per directory, `.ascattributes` if present, else `.gitattributes` (tree and worktree; the files are never merged), then `<common>/info/attributes` and `core.attributesFile`.
- **Honoured natively:**
  - `binary`, `-diff`, `diff` (no driver) and `-text` in diff output (`Binary files … differ`);
  - `merge=union`, `merge=ours`, `merge=binary` and `-merge` in merges (§4.4);
  - `export-ignore` and `export-subst` in `archive`;
  - `diff=<driver>` with `diff.<driver>.textconv` or `.command`, and `merge=<driver>` with `merge.<driver>.driver`, by running the configured command as git does;
  - `linguist-generated` (classification, §8.10).
- **Ingest-side normalisation, as `git add` does:** for paths whose attributes are `text`, `text=auto` (when the content is not binary by git's heuristic) or `eol=*`, `asc add`, `commit -a` and status hashing convert CRLF to LF. A working file and the same file added with git therefore give the same blob.
- **Not applied at checkout (documented, accepted deviation):** `eol`, `crlf`, `text` conversion on checkout. Working files always hold the committed bytes. VFS sizes come from tree metadata, so a conversion would break placeholder sizes and turbo's stat hashing. A global `core.autocrlf=true` is ignored in asc workspaces.
- **Unsupported:** `ident`, `working-tree-encoding` and `filter=<x>` other than `lfs`. `asc add` of such a path fails with `fatal: asc: attribute '<a>' is not supported` (exit 128).
- **`filter=lfs` is ignored [audit: lfs-filter-add-fails]:** the content is stored natively (chunked over 4 MiB) and `asc add` prints `warning: asc: ignoring filter=lfs for <path> (large files are native)` once per command. Existing pointer files stay ordinary blobs (§2.5).
- When a repo tracks a `.gitattributes` that uses an unapplied or unsupported attribute, `asc clone` and `asc doctor` print one `warning: asc: …` naming the attribute and the affected paths. The warning also explains the `autocrlf` rule when a global `core.autocrlf=true` is present. The compat allow-list (§17.1) records the deviation.

#### 2.4.2 Ignore files: `.ascignore` first, `.gitignore` as fallback (user decision)

- **Per-directory precedence:** if a directory contains a `.ascignore`, asc uses it and does not read that directory's `.gitignore`; otherwise asc reads `.gitignore`. Files are never merged.
- **Syntax and semantics are gitignore's** (negation, `**`, anchoring, trailing `/`, escaping, nested files scoped to their directory). One parser (`ignore` crate rules) serves both names.
- **Repo-local excludes [audit: asc-info-exclude-location]:** `.asc/info/exclude` is `<worktree root>/.asc/info/exclude`. The path `.asc/info/` is never tracked: status, add and ls-files treat it as ignored, like `.git`. asc reads it if it exists, else `<common>/info/exclude` (the `.git/info/exclude` equivalent).
- **Global excludes:** `asc.excludesFile`, default `~/.config/asc/ignore`; when that file does not exist, git's `core.excludesFile`.
- **Commands [audit: migrate-ignore-default-and-stale-text]:** `asc check-ignore -v` reports the file and line that matched (git's format). `asc migrate ignore [--move]` copies each `.gitignore` to `.ascignore` (default), so tools that read `.gitignore` directly (ripgrep, prettier, eslint, biome) keep working; `--move` renames. `asc doctor` warns when a directory has both files with different content.
- **`asc init` and the shadow warning [audit: ascignore-shadows-gitignore]:** `asc init` writes a `.ascignore` only when the directory has no `.gitignore`, with the first line `# asc: replaces .gitignore in this directory`. `git init` through the shim writes none (§11.4). When one directory holds both files and their contents differ, `asc status`, `asc add` and `asc check-ignore` print once per command on stderr `warning: asc: <dir>/.gitignore is not read because <dir>/.ascignore exists (rename or merge it: asc migrate ignore)`, and `asc doctor` lists every such directory.
- Git export and the Forgejo bridge carry both files as ordinary tracked content. With no `.ascignore` anywhere, behaviour is exactly git's, so the compat suite (§17.1) is unaffected.

### 2.5 Trees

```rust
pub struct Tree { pub entries: Vec<Entry> }       // strictly ascending by name bytes; root may be empty, others not
pub struct Entry {
    pub name: Name,
    pub kind: Kind,
    pub id: [u8; 32],          // File/Exec/Symlink: FileId; Dir: TreeId; Gitlink: all zero
    pub size: u64,             // File/Exec/Symlink: byte length of the git blob; Dir: sum of descendant blob sizes; Gitlink: 0
    pub git: GitOid,           // git blob oid / git tree oid / gitlink commit oid
}
pub enum Kind { File = 0, Exec = 1, Symlink = 2, Dir = 3, Gitlink = 4 }   // postcard variant index as listed
```

- A tree depends only on content, never on path or history.
- `git` of a `Dir` entry is the git oid of that subtree **as git saw it**. For canonical trees this equals `sha1(render_git_tree(child))`. For imported non-canonical git trees (unusual modes, odd ordering) it is the original oid, and the original git tree body (no header) is stored as a **git tree override** object `o/<oid>` (§3.1, §3.3). The exporter uses the override whenever `sha1(render) != git`.
- The git oid of the root tree is not stored in `Tree`. It is `sha1(render_git_tree(root))`, except for imported commits that keep raw bytes (§2.9), whose tree line is authoritative and whose root override is stored like any other.
- **No LFS (user decision) [audit: user decision No-LFS]:** asc has no LFS support. Git LFS pointer files in imported history are ordinary small blobs, kept byte-exact, so git SHAs and the mirror stay identical; working copies show the pointer text. LFS content is never fetched or migrated; ASC's 25.6 GiB LFS store is archived once to a cold S3 bucket, outside asc (§14.3).
- Large files are native chunked content (§2.4), with no setup needed.

### 2.6 Changesets

```rust
pub struct Changeset {
    pub parents: Vec<ChangesetId>,               // first parent first; empty for a root changeset
    pub root: TreeId,
    pub author: Signature,
    pub committer: Signature,
    pub message: Vec<u8>,                        // raw bytes
    pub extra: Vec<(Vec<u8>, Vec<u8>)>,          // sorted map; keys are ASCII, namespaced (below)
    pub changes: Vec<(RepoPath, Change)>,        // sorted map; relative to parents[0] (or the empty tree)
}
pub struct Signature { pub name: Vec<u8>, pub email: Vec<u8>, pub time: i64 /*unix s*/, pub tz_min: i16 }
pub enum Change {
    Write { kind: Kind /* File|Exec|Symlink */, file: FileId, size: u64, git: GitOid,
            copy_from: Option<RepoPath> },
    Gitlink { commit: GitOid },
    Delete,
}
```

**Validation (MUST, on every ingest: local commit, push, import, server mutation):**

1. Every parent exists; `parents` has no duplicates.
2. `apply(tree(parents[0]) or empty, changes) == root`, where `apply` builds the new tree along changed spines (§4.1).
3. `changes == leafdiff(tree(parents[0]) or empty, root)` exactly: every changed leaf appears once, nothing unchanged appears, and a file replaced by a directory (or the reverse) appears as `Delete` of the old leaf plus `Write`s of the new leaves. Therefore `changes` is a pure function of the two roots plus `copy_from` annotations, and two producers of the same commit always agree.
4. Every `Write.file` exists, and its `size` and `git` (blob SHA-1) match the content (§8.4 verifies on the server).
5. `copy_from`, if present, names a path that exists in `tree(parents[0])`.
6. Merge changesets record changes relative to `parents[0]` only (Scion/Bonsai).

**`extra` keys** (others are rejected):

| Key | Value | Meaning |
|---|---|---|
| `git.commit` | raw git commit object body | Imported commit that is not canonical (§2.9). Export emits these bytes verbatim. |
| `git.gpgsig` | signature bytes (armored) | Signature over the canonical git commit bytes without the `gpgsig` header. Rendered as the `gpgsig` header. |
| `asc.snapshot` | empty | Snapshot changeset (stash); refused on protected refs. |

### 2.7 Tags and refs

```rust
pub struct TagRecord { pub target: ChangesetId, pub name: Vec<u8>, pub tagger: Option<Signature>,
                       pub message: Vec<u8>, pub extra: Vec<(Vec<u8>, Vec<u8>)> } // extra: "git.tag" raw, "git.gpgsig"
pub enum RefTarget { Changeset(ChangesetId), Tag(TagId) }   // stored as 33 bytes: 0x01|0x02 ‖ id
```

Ref names follow `git check-ref-format`. Namespaces: `refs/heads/*`, `refs/users/<owner-id>/heads/*` (user refs, server only, §21.5.1), `refs/tags/*`, `refs/remotes/*` (local only), `refs/stash` (local only), `refs/pull/<n>/head` (Forgejo), `refs/heads/agent/*` and `refs/snapshots/*` (may carry an expiry, §8.8), `refs/heads/green/*` (post-merge, §12.6), `refs/bisect/*` (local only). `refs/for/*` is never stored; it is an AGit push target (§13.5).

**Ref scope (Scion principle 6) [audit: 6-ref-scoping, branch-scope-model]:**
- Creating a changeset never touches refs (§8.7). A server ref is one SQL row with per-row CAS (§3.4); no cross-server agreement exists. A ref costs one row and one `ListRefs` entry.
- Local branches are machine-local (§6.1). In a branchless repo (the default, §21) a pushed non-shared branch is not a global ref: it becomes the pusher's user ref `refs/users/<owner-id>/heads/<n>` (§21.5.1).
- Global refs that every client follows by default are the repo's `shared_refs`: `master`, `release/*`, `green/*` and tags (§21.5.1). In a branch-mode repo (`repos.branchless=false`, or Stage 0 and 1) they also include developer branches pushed for review.
- Agent branches live under `refs/heads/agent/*` with a TTL (§8.8). `asc clone` writes the fetch refspecs `+refs/heads/*:refs/remotes/origin/*` and `^refs/heads/agent/*` (git ≥ 2.29 negative refspec syntax). `asc fetch origin agent/<name>`, or an explicit refspec, fetches one agent branch.
- `ListRefs` with a prefix is paged (`limit` default 1,000).
- Agents that need no branch use `Ingest`, or `Commit` without a ref, and refer to changesets by id.

### 2.8 Path rules by context

| Context | Rule |
|---|---|
| Decoding any object | `Name`/`RepoPath` validity (§2.3) |
| Importing git history | Nothing more. History is never rejected for Windows names or case collisions **[fix: missing-from-all]** |
| New native changesets (local commit, push, server mutation) in a repo with `windows_safe = true` (default) | For each path **added or changed** by the changeset: `windows_valid` for every component, `checkout_safe`, and no case-fold collision with a sibling in the resulting tree. Pre-existing violations elsewhere are not re-checked. |
| Materialising (VFS, plain) | `checkout_safe` always. On Windows, entries that are not `windows_valid`, or that case-collide with an earlier sibling (in byte order), are not projected. They are listed by `asc status` under "unrepresentable on this OS", keep their index entries with the skip-worktree bit, and are never reported as deleted. |

### 2.9 Git mapping (`asc-core::git`)

Git object rendering is exact and golden-tested against `git hash-object`, `git mktree` and `git commit-tree`.

- **Blob:** `sha1("blob " ‖ ascii(len) ‖ 0x00 ‖ bytes)`.
- **Tree render:** entries sorted by git order (name bytes, with directories compared as `name ‖ "/"`). Each entry is `mode ‖ " " ‖ name ‖ 0x00 ‖ oid[20]`, with modes `100644` File, `100755` Exec, `120000` Symlink, `40000` Dir, `160000` Gitlink. Header `tree <len>\0`.
- **Commit render (canonical):**

```
tree <root git oid hex>\n
parent <gitcommit(p) hex>\n                       (one per parent, in order)
author <name> <<email>> <time> <tz>\n             (tz = sign ‖ HH ‖ MM from tz_min, e.g. +0100, -0530, +0000)
committer <name> <<email>> <time> <tz>\n
asc <base64(0x01 ‖ postcard(AscGitExtra))>\n       (only when AscGitExtra is non-empty; version byte, §2.2a)
gpgsig <line 1>\n <line 2>\n …                    (only when extra has git.gpgsig; continuation lines start with one space)
\n
<message bytes verbatim>
```

```rust
pub struct AscGitExtra { pub copies: Vec<(RepoPath, RepoPath)>, pub snapshot: bool } // (path, copy_from), sorted
```

  The `asc` header carries asc-only data through git, so a native changeset exported to git and re-imported has the same id.

- **Import canonicalisation (deterministic, used by every importer):** parse the raw commit into tree, parents, author, committer, optional `asc` header, optional `gpgsig`, and message. Build the canonical changeset (with `git.gpgsig` and the copies from the `asc` header). If `render(canonical) == raw`, store it **without** `git.commit`. Otherwise store `extra["git.commit"] = raw`. Tags work the same way with `git.tag`. Trees: if `render(tree) != raw`, store the override.
- **`gitcommit(cs)`** (derived, §5): `sha1` of `extra["git.commit"]` if present, otherwise of the canonical render.
- **Signing:** `asc commit -S` renders the canonical commit without `gpgsig`, signs it with git's configured program (`gpg.format`, `gpg.program`, `gpg.ssh.program`, `user.signingkey`), and stores `git.gpgsig`. Forgejo and git verify these signatures unchanged.

---

## 3. Storage (crate `asc-store`)

### 3.1 Keys

```rust
pub enum ObjKey {
    File(FileId),            // f/   inline file bytes (size ≤ 4 MiB)
    ChunkList(FileId),       // l/   ChunkList envelope (size > 4 MiB)
    Chunk(ChunkId),          // k/
    Tree(TreeId),            // t/
    Changeset(ChangesetId),  // c/
    Tag(TagId),              // g/
    GitTreeOverride(GitOid), // o/   git tree object body without the "tree <len>\0" header; valid iff sha1("tree " ‖ ascii(len) ‖ 0x00 ‖ body) == oid
    ChangeMeta(MetaId),      // m/   ChangeMeta envelope (kind 6, §20.2); id = BLAKE3 of the envelope
    Derived { name: &'static str, version: u32, id: [u8; 32] },   // d/<name>.v<version>/
}
```

`File` vs `ChunkList` is decided by size, which every referrer knows from its `Entry`/`Change`.

**Stored value** (bucket and wire): `codec: u8 ‖ data`, codec `0` = raw and `1` = one zstd frame (level 3, content size set). A writer uses zstd only if the frame is < 90% of raw. Readers accept both. The stored bytes are the wire bytes; only the consumer decodes.

### 3.2 Traits

```rust
#[async_trait]
pub trait Objects: Send + Sync {
    async fn get(&self, key: &ObjKey) -> Result<Option<Bytes>>;                  // decoded data (codec removed), verified
    async fn get_stored(&self, key: &ObjKey) -> Result<Option<Bytes>>;           // stored bytes (codec ‖ data); not decoded, not verified
    async fn get_range(&self, key: &ObjKey, r: Range<u64>) -> Result<Option<Bytes>>; // File/Chunk only
    async fn put(&self, key: &ObjKey, data: Bytes) -> Result<()>;               // create-if-absent; AlreadyExists == Ok
    async fn has_many(&self, keys: &[ObjKey]) -> Result<Vec<bool>>;
    async fn delete(&self, key: &ObjKey) -> Result<()>;                          // GC only
    fn list(&self, prefix: KeyPrefix) -> BoxStream<'_, Result<(ObjKey, SystemTime)>>; // GC only
}

#[async_trait]
pub trait Refs: Send + Sync {
    async fn get(&self, repo: RepoKey, name: &RefName) -> Result<Option<(RefTarget, u64 /*version*/)>>;
    async fn list(&self, repo: RepoKey, prefix: &str) -> Result<Vec<(RefName, RefTarget, u64)>>;
    async fn update(&self, repo: RepoKey, txn: &RefTxn) -> Result<(), RefConflict>; // atomic where supported (§3.4)
    async fn log(&self, repo: RepoKey, name: &RefName, limit: u32) -> Result<Vec<RefLogEntry>>;
}
pub struct RefTxn { pub updates: Vec<RefUpdate>, pub actor: Actor, pub reason: String, pub op_id: Uuid }
pub struct RefUpdate { pub name: RefName, pub expect: Expect, pub new: Option<RefTarget> /* None = delete */ }
pub enum Expect { Any, Absent, Value(RefTarget) }
```

**Verification happens where an object is consumed (MUST) [audit: 16-stored-bytes-serving]:** `get` verifies `File`, `Chunk`, `Tree`, `Changeset`, `Tag` and `ChangeMeta` values against their id before returning. A mismatch is `Error::Corrupt(key)`. The caller treats the object as missing, and caches delete the file **[graft: integration] [fix J1: cache poisoning]**.
- Every client and CI `Tiered` fill, and any server code that decodes an object for its own work (diff, merge, blame, derivation, export), uses `get` and so verifies.
- `get_stored` returns the stored bytes untouched: `BucketObjects` returns the object body, `CacheDir` returns `0x00 ‖ file bytes`, and `Tiered` fills A from B's stored bytes. The server's `GetObjects` and the cache role (§7) serve `get_stored` bytes from memory, disk or the bucket without decompressing, re-compressing or hashing. Each object was verified once at `PutObjects` (§8.4), and the server's own bucket is trusted (§15.4).
- `asc fsck` and `RepoAdmin.verify` re-hash stored objects offline.

### 3.3 Implementations

| Impl | Backing | Notes |
|---|---|---|
| `BucketObjects` | `object_store` 0.14.2: `AmazonS3` (S3, MinIO, R2, Garage, Ceph RGW), `LocalFileSystem`, `InMemory` | Stores codec-prefixed values. `put` uses `PutMode::Create`. `AlreadyExists` / 412 / 409 → `Ok`. On `NotImplemented`, 501, or a 400 that names the conditional header, the store remembers `create_unsupported` and uses `PutMode::Overwrite` from then on. That is safe because keys are content-addressed and immutable **[fix: missing-from-all]**. Multipart for values > 32 MiB. |
| `CacheDir` | plain files | Machine cache and machine store (§3.5). Stores **decoded** data so content files can be copied, reflinked or passed through. |
| `Tiered<A, B>` | read A, on miss read B, then fill A; writes go to A | Client view of a repo: `Tiered<StoreSet, Remote>` |
| `RemoteObjects` | gRPC `GetObjects`/`PutObjects`/`HasObjects` | In `asc-rpc` |
| `SqlRefs` | `sqlx` 0.9 `AnyPool` over SQLite or Postgres | Atomic multi-ref CAS in one transaction (§3.4) |
| `BucketRefs` | refs inside a bucket, using create-only puts | Single-ref atomic only; enabled only after the probe passes (§3.4) |

**Bucket layout** (one namespace per repository **[fix: missing-from-all, cross-fork reads]**):

```
<root>/ns/<ns>/f/<h2>/<hex64>          inline file
<root>/ns/<ns>/l/<h2>/<hex64>          chunk list
<root>/ns/<ns>/k/<h2>/<hex64>          chunk
<root>/ns/<ns>/t/<h2>/<hex64>          tree
<root>/ns/<ns>/c/<h2>/<hex64>          changeset
<root>/ns/<ns>/g/<h2>/<hex64>          tag
<root>/ns/<ns>/o/<h2>/<hex40>          git tree override
<root>/ns/<ns>/m/<h2>/<hex64>          change meta (§20.2)
<root>/ns/<ns>/d/<name>.v<N>/<h2>/<hex64>   derived (recomputable)
<root>/ns/<ns>/rl/<refname>/<v:020>    ref log entries (BucketRefs only)
<root>/ns/<ns>/r/<refname>             ref hint (BucketRefs only)
<root>/turbo/<repo-slug>/<scope>/<hash>[.meta]   turbo remote cache (§12.4)
<root>/actions/<owner>.<repo>/<cache|artifact>/<scope>/<pct(key)>   Actions manifests (§12.7)
<root>/seeds/<repo-slug>/<changeset>.tar.zst, latest   seed snapshots (§12.3)
```

`<h2>` is the first two hex characters. `<ns>` is a 16-byte random id (hex) assigned at repo creation. A bucket remote URL (`s3://…`, `file://…`) addresses one namespace directly: its URL prefix plays the role of `<root>/ns/<ns>`. There are no packfiles. Every object can be fetched alone, so transfers resume by asking again for whatever is missing.

**Namespaces and forks.** Objects are only ever read within the namespace of the repository named in the request. A Forgejo fork gets a new namespace. `Fork` copies the reachable closure server-side, using S3 `CopyObject` or a local reflink, as a background job. The fork is readable when the copy completes (minutes for ASC). Cross-namespace operations (PR merges from forks, `refs/pull/<n>/head`) first call `CopyClosure` (§7), authorised against both repositories. This closes the confirmation attack ("does this content exist in a private fork?"). Cross-fork dedup is therefore physical only: `CopyObject`/reflink copies, and Garage's block-level dedup. Logical dedup across a fork network is a measured v1.x item (ASC has one repository and no forks). This defers a checklist v1 item and is recorded as §19 decision 6 **[audit: fork-dedup-deviation-unsigned]** **Derived index data is never copied [audit: codeintel-folded]:** `Fork` and `CopyClosure` copy the reachable content closure only, never `d/tdocs.v1`, `d/tags.v1`, `d/chunks.v1`, `d/ciseg.v1`, `d/civec.v1`, `d/ciidx.v1` or any `ci_*` row (§23.2.4); a fork starts un-indexed and backfills its own heads.

### 3.4 Ref stores and their atomicity

**`SqlRefs`** (server; and machine-local refs in `state.db`):

```sql
CREATE TABLE refs (repo_id BIGINT NOT NULL, name TEXT NOT NULL, target BLOB/BYTEA NOT NULL, -- 33 bytes
                   version BIGINT NOT NULL, expires_at BIGINT NULL,                       -- unix s
                   PRIMARY KEY (repo_id, name));
CREATE TABLE ref_log (seq BIGINT PRIMARY KEY /*autoinc*/, repo_id BIGINT, name TEXT, old BLOB NULL, new BLOB NULL,
                      actor TEXT, via TEXT, op_id BLOB, at BIGINT);
```

Update: in one transaction, for each update, `UPDATE refs SET target=$new, version=version+1 WHERE repo_id=$r AND name=$n AND version=$v` (or `INSERT` for `Absent`, `DELETE … WHERE version=$v` for deletes). Any zero-row result rolls back and returns `RefConflict { name, actual }`. Server-only callers append more statements to the same transaction (§8.3).

**`BucketRefs`** (serverless shared bucket) **[fix J1/J2: serverless multi-writer, ABA, reflog]**:

- **Deferred (post-v1, unit U30, off the critical path) [audit: bucketrefs-defer]:** v1 bucket remotes use `--refs local` (single-writer) by default. Multi-writer teams use `asc serve`. `asc init --store` without `--refs` uses `local` until U30 ships. The design below is kept so U30 can be built without redesign.
- The **ref log is the truth.** Entry `rl/<ref>/<v>` (v zero-padded to 20 digits, starting at 1) holds `postcard(BucketRefEntry { old: Option<RefTarget>, new: Option<RefTarget>, op_id: [u8;16], actor: Vec<u8>, at: i64 })`. The current value is the entry with the highest `v`.
- **CAS = create-only put** of `rl/<ref>/<v+1>` after reading entry `v` and checking that `entry_v.new == expected`. Only one writer can create a given `v+1`. On `AlreadyExists`, read the entry: if its `op_id` equals mine, the earlier attempt succeeded (a retried create), so return success. Otherwise return `RefConflict`. Version numbers never repeat, so there is no ABA.
- `r/<ref>` holds the latest known `v` as a **hint**, written with a plain overwrite after a successful CAS. Readers start at the hint and probe `v+1, v+2, …` until a missing key. Listing refs lists `r/` and then resolves each one.
- Atomicity covers one ref per update. A multi-ref push applies refs in order and reports partial success explicitly.
- **Probe** (`asc doctor --store`, and automatically on `asc init --store s3://…` and on the first push to a bucket remote): put key `probe/<uuid>` with `Create` twice. The second put MUST fail with `AlreadyExists`. If it succeeds (header ignored) or the store errors with `NotImplemented`, `BucketRefs` is refused for that bucket with a message naming the store. The bucket can still hold objects, and refs then need a server or the single-writer mode below. Garage v2 is expected to fail the probe.
- **Single-writer fallback:** `asc init --store <bucket> --refs local` keeps refs in the machine's `state.db`. It is explicitly single-machine and is labelled as such in `asc remote -v`. Workspaces on that machine (one `$ASC_HOME`) share the refs. The bucket's refs are stored under a dedicated `state.db.repos` row per bucket (`remote_kind = bucket-local-refs`, `remote_url` = the normalised bucket URL, `ns` = the bucket's namespace), as `refs/heads/*` and `refs/tags/*`. Push CASes those rows (`BucketRemote::update_refs`). Fetch copies them into the cloning repo's `refs/remotes/<remote>/*`. Each clone is its own `repos` row with its own local branches, as with git **[audit: bucket-local-refs-storage-undefined]**. The first push writes the create-only bucket key `refs-local` holding this machine's id (`state.db.meta.machine_id`). A clone or fetch of a bucket remote from a machine that does not hold its refs (the key names another machine) fails with `fatal: asc: refs for <url> are local to another machine; use an asc server or --refs shared (U30)` (exit 128) **[audit: bucket-local-refs-two-clones]**.

### 3.5 Machine layout (`$ASC_HOME`)

`$ASC_HOME` = env `ASC_HOME`, else global git config `asc.home`, else `%LOCALAPPDATA%\asc` (Windows) or `${XDG_DATA_HOME:-~/.local/share}/asc`. ASC sets `D:\asc`, on the same volume as bun's cache. Inside a workspace, asc derives `$ASC_HOME` from the gitfile target (`<home>/wc/<ws>/git` → `<home>`); `ASC_HOME`/`asc.home` apply only outside workspaces **[audit: ci-later-steps-asc-home-and-runner-mounts]**.

```
cache/{f,l,k,t,c,g,o,m,d}/<h2>/<hex>   evictable decoded copies of remote objects (CacheDir)
store/{f,l,k,t,c,g,o,m,d}/<h2>/<hex>   objects that exist only here: local-only repos, not-yet-uploaded commits, staged content. Never evicted by LRU; the one removal is the hidden-draft expiry (§21.2.4: objects reachable only from hidden draft heads past `asc.draft.hiddenDays`). Un-uploaded objects of visible heads are never removed.
tmp/                                  staging for atomic writes (same volume)
state.db                              SQLite: repos, local refs, ref log, workspaces, staging and stage sets, journal, git map, history boundary, entry times, intents
repos/<repo-id>/.git/                 common dir per repo: git-format `config` (remotes, upstreams, local settings), `info/exclude`,
                                      `hooks/` (default hooks dir). No objects and no refs (§9.4)
repos/<repo-id>/.turbo/cache/         machine-wide turbo cache shared by the repo's workspaces (§9.4)
repos/<repo-id>/git-remote.git/       git-remote mode only: a private bare repo that real git fetches into and pushes from (§6.3)
wc/<ws-id>/git/                       synthetic per-workspace gitdir (§9.4): HEAD, index, commondir, asc-workspace, in-progress state files
wc/<ws-id>/overlay/                   FUSE only: modified file bodies
wt/<ws-id>/                           mount roots for worktrees requested inside another workspace (§9.3)
redirect/<ws-id>/<path>               FUSE only: redirected directories
tools/                                reserved for pinned helper tools (none in v1)
run/                                  daemon pid, Unix socket fallback, real-git resolution cache
log/daemon.log
```

`$ASC_HOME` and `state.db` are implemented in crate `asc-state` (it depends on `asc-store` for `SqlRefs` and on `asc-engine` for `GitMapSink`). `asc-git`, `asc-wc`, `asc-vfs`, `asc-ci` and `asc` depend on it **[audit: crate-cycle-statedb]**.

- **Atomic write:** write to `tmp/<uuid>`, fsync, then rename to the final path. If the final path exists, delete the temp file and succeed. Readers never see partial files. Files are opened for read with `FILE_SHARE_DELETE` on Windows, so eviction never blocks readers.
- **Content files** (`f/`, `k/`) hold raw bytes. A chunked file also gets an assembled `f/` copy when it is first hydrated, if it is ≤ 1 GiB.
- **LRU** uses file mtime. A read touches mtime when it is older than 24 h. The daemon (or `asc gc`) evicts `cache/` files with the oldest mtime once the cache exceeds `asc.cache.maxSize` (default 20 GiB), down to 80% of that.
- **Demotion:** after a successful push or commit-cloud upload (§21.3.1), the uploaded objects are renamed from `store/` to `cache/`.
- **Store set:** `StoreSet` reads `store/` then `cache/`, and writes new local objects to `store/`.
- **Cache keys [audit: ci-tier-stack-config-undefined-unowned]:** `asc.cache.dir` replaces `$ASC_HOME/cache` as the evictable cache. `asc.cache.readOnly` (multi-valued) adds read-only `CacheDir` tiers in front of it, verified on read and never written. Every asc process builds `Tiered<readOnly…, StoreSet{store, cache}, remote>` from these keys; `Ctx` builds this stack itself in every process, with no per-mode override **[audit: object-stack-selector-single-owner-blocks-remote-tier]**. The remote tier is the default remote's object source: `BucketObjects` for a bucket remote, `RemoteObjects` (honouring the §7 `objects` tier list) for a server remote, and none for local-only and git-remote modes, whose objects come from import. An `asc.cache.readOnly` tier never writes and never deletes; a corrupt file there is reported as a miss.
- **Storage budgets [audit: user decision efficiency]:** each object is stored once per machine and shared by all workspaces, worktrees and repos; `cache/` is LRU-bounded; hydrated files are released by dehydration (§10.2.9); there are no packfiles and no cruft. `asc debug stats` reports bytes per workspace, which is about 0 before hydration (only the synthetic `.git` index and rows).

### 3.6 Garbage collection (server and bucket; unit O01, not needed for ASC v1)

- Objects are never deleted on the write path.
- **Two-mark rule [fix: missing-from-all, concurrent writers]:** a sweep deletes an object only if it was unreachable in two marks taken at least 24 h apart, and its `LastModified` is older than 14 days. Roots for each mark are: all refs; every changeset object whose `LastModified` is within 14 days; retained snapshot refs; every changeset named by `change_versions` (§20.4; a `m/` object follows its changeset); turbo objects (by their own retention); the FileIds named by every unexpired Actions manifest (§12.7); every `cloud_heads.cs`, visible or hidden within the grace period (§21.2.4). A push in progress during mark 1 that re-references an old object uploads its changeset before mark 2, which makes the object reachable again. Pushes taking longer than 24 h are not supported. The client re-uploads on `FAILED_PRECONDITION: missing objects`.
- **Purge [audit: branchless-folded]:** an object listed in `gc_purge` (§21.3.7) that no ref or head reaches is swept at the next run without the 14-day age rule.
- Server marks are stored in SQL (`gc_marks`). Bucket-mode GC (`asc gc --store <url>`) keeps its marks in the bucket under `gc/<run>/` and refuses to run concurrently, using a `gc/lock` create-only key with a 6 h lease.
- **Sweep fencing [audit: gc-sweep-race-and-pitr-roots]:**
  - *Server:* `UpdateRefs` holds a shared `pg_advisory_xact_lock(asc_gc)` (SQLite: an in-process RwLock) from pipeline step 4 until the commit in step 8. Each sweep batch takes the lock exclusively, re-marks from refs and from changesets ingested since mark 2 started, removes newly reachable keys from the batch, and only then deletes.
  - *Bucket mode:* the sweep first creates `gc/freeze` (create-only; body = start time), waits 10 min, re-marks from all refs, deletes, then removes `gc/freeze`. A bucket-mode push reads `gc/freeze` immediately before its ref CAS. If the key is present, it aborts before the CAS with `fatal: asc: bucket is being garbage-collected; retry after <t>`.
  - *PITR roots:* roots also include every `new` target in `ref_log` (server) or in ref-log entries (bucket) newer than `asc.gc.pitrDays` (default 14; MUST be ≥ the Postgres PITR window).

---

## 4. Engine (crate `asc-engine`)

`Engine<O: Objects, R: Refs>` is the only implementation of repository logic. It runs inside the CLI (local operations, local-only and bucket remotes) and inside `asc serve`.

### 4.1 Trees (`tree.rs`)

- `apply(root, changes) -> TreeId`: group changes by parent directory, rebuild only the touched spines bottom-up, write new `Tree` objects, prune empty directories, and recompute each new `Entry`'s `size` and `git` (a `Dir`'s `git` = sha1 of its canonical render). Cost is O(changed paths × depth).
- `leafdiff(a, b, pathspec) -> Vec<(RepoPath, Change)>` with **hash pruning**: equal `TreeId`s are skipped. File-to-directory replacement produces a `Delete` plus `Write`s.
- `lookup(root, path)`, `walk(root, pathspec)`, glob matching with `globset`.
- Oracle property test: `apply(a, leafdiff(a, b)) == b`, and pruned diff equals a naive full-walk diff.

### 4.2 Commits and revisions (`commit.rs`, `revspec.rs`)

- `build_changeset(parents, index_entries | changes, sigs, message, extra)`: computes changes as `leafdiff(parent root, new root)`, attaches `copy_from` from rename records, writes the tree, and validates (§2.6).
- Revspec grammar (git subset, all supported): full or abbreviated asc id; full or abbreviated git SHA (through the git map); ref names with git's DWIM order (`<n>`, `refs/<n>`, `refs/tags/<n>`, `refs/heads/<n>`, `refs/remotes/<n>`, `refs/remotes/<n>/HEAD`); `HEAD`, `@`; `<rev>^`, `^N`, `~N`; `<rev>^{tree}`, `^{commit}`, `^{}`; `@{u}`, `@{upstream}`, `<branch>@{u}`; `@{N}` (reflog); `:/<regex>`; `<rev>:<path>`; ranges `A..B`, `A...B`, `^A B`, `<rev>^@`, `<rev>^!`.

### 4.3 Graph and history (`graph.rs`)

- An in-memory graph per repo, `ChangesetId -> (gen: u32, parents)`, where `gen = 1 + max(gen(parents))`. It is loaded lazily: from `cs_graph` on the server, from the `gen` derived value locally.
- `merge_base(a, b)`: a two-frontier walk in descending generation order (git's paint-down algorithm), jumping along first-parent chains with `skip.v1` pointers (§5) whenever the jump target's generation is still ≥ the other frontier's. `is_ancestor(a, b)`: from `b`, follow skip pointers while the target's `gen ≥ gen(a)`, then walk, pruning nodes with `gen < gen(a)`. On land branches the mainline index answers in O(1) (§5.1).
- `log(range, pathspec, first_parent, limit, cursor)`: generation-ordered walk. The path filter uses each changeset's `changes` keys (prefix match), so it costs no tree reads. `--follow` uses `copy_from`. On land branches path-filtered log uses the path index (§5.1).
- **History traits [audit: historyapi-cycle, historyapi-impl-split]:** `HistoryApi` (log, merge_base, is_ancestor, diff, resolve, git_map) in `asc-engine::history`; `BlameApi` (blame) in `asc-engine::blame`; `ProjectsApi` (project_graph, affected, project_hashes) in `asc-engine::projects`. `Engine` implements each trait in the module that defines it. `asc-rpc::RpcClient` implements `HistoryApi` and `BlameApi` (`asc-rpc::client`) and `ProjectsApi` (`asc-rpc::projects`).
- **History boundary [audit: clone-history-o-history]:** a local walk (log, merge-base, ahead/behind, blame, describe) that reaches a changeset recorded in `history_boundary` (§6.3) continues through the remote's `HistoryApi` (`Log`/`MergeBase`/`IsAncestor` with the boundary ids). Output is identical to a walk over full local history.
- Line diff (`diff/`): `imara-diff` 0.2 (`Myers`, `MyersMinimal`, `Histogram`) plus a native patience implementation (`diff/patience.rs`), with indent heuristic post-processing. Output formats: `--name-only`, `--name-status`, `--stat`, `--numstat`, `--shortstat`, `--summary`, `--raw`, unified patch (`-U<n>`, rename headers, mode lines, `Binary files … differ`, `\ No newline at end of file`), `--word-diff=plain`.
- **Diff contract [audit: diff-byte-equality]:**
  - asc honours `diff.algorithm` and `--diff-algorithm|--histogram|--patience|--minimal`, plus `diff.indentHeuristic`, `diff.context`, `diff.interHunkContext`, `diff.noprefix`, `diff.mnemonicPrefix`, `diff.relative` and `color.diff.*`.
  - Output with `--raw`, `--name-only`, `--name-status`, `-z`, `--summary` and the `--stat`/`--shortstat` file lists MUST be byte-equal to git on 100% of the golden corpus (pairs taken from ASC history).
  - Unified patch and `--numstat` MUST be byte-equal on 100% of the corpus for each configured algorithm. Any unequal case is a bug and is recorded in the corpus's exceptions file, which MUST be empty at the phase-1 exit.
  - Every patch passes `git apply --check`. Forgejo depends only on this property **[graft: integration]**.
- Renames: exact (equal `FileId`) first, then similarity among added/deleted files only, capped by `diff.renameLimit` (default 1000). Off unless `-M`, or `diff.renames=true` (git's default for porcelain).

### 4.4 Merge, rebase, land (`merge/`, `rebase.rs`, `land.rs`)

- **Tree merge** of base→ours and base→theirs is the union of two change sets. A path changed on one side takes that side. A path changed identically on both sides takes it. A path changed differently gets a content 3-way merge with the `gix-merge` (pinned to the `gix` 0.89 family) builtin text driver, which produces git's markers in `merge`, `diff3` and `zdiff3` styles. Only both-sides paths read content. File/directory and case-fold conflicts are detected on changed paths only. Binary conflicts take ours and record the conflict.
- Merge attributes (§2.4.1) apply per path: `merge=union|ours|binary`, `-merge`, and configured `merge.<driver>.driver` commands.
- **Conflicts** in a working copy: markers in the files and stages 1/2/3 in asc's staging area (§9.5).
- **In-progress state uses git's on-disk format in the workspace gitdir, and that format is the only store [audit: git-state-files]:**
  - merge writes `MERGE_HEAD`, `MERGE_MSG`, `MERGE_MODE` and `AUTO_MERGE`;
  - cherry-pick writes `CHERRY_PICK_HEAD`; revert writes `REVERT_HEAD`; a multi-commit pick or revert uses `sequencer/{todo,head,opts}`;
  - rebase uses `rebase-merge/` (`head-name`, `onto`, `orig-head`, `git-rebase-todo`, `done`, `msgnum`, `end`, `interactive`, `stopped-sha`, `amend`, `author-script`, `message`, `autostash`) and `REBASE_HEAD`;
  - `ORIG_HEAD` is written by reset, merge, rebase and pull; `--squash` writes `SQUASH_MSG`; every commit writes `COMMIT_EDITMSG`;
  - bisect writes `BISECT_START`, `BISECT_LOG`, `BISECT_TERMS`, `BISECT_EXPECTED_REV` and `refs/bisect/*`.

  asc reads and writes these files with git's semantics, so scripts that inspect them (lint-staged, commitlint, shell prompts) work. Ids in them are git SHA-1s. `state.db.ws_ops` keeps only pending `asc mv` renames.
- **Rebase of changeset `C` onto `T`:** if `paths(C)` is disjoint from `paths(base(C)..T)` (§4.5), the result is `C` with `parents = [T]`, the same `changes`, and `root = apply(T.root, C.changes)` (metadata-only, no content reads) **[graft: performance]**. Otherwise only the intersecting files are merged. A rebased changeset drops `extra["git.gpgsig"]` and `extra["git.commit"]` (a raw body names the old parents), and is re-signed locally when `commit.gpgSign` or `-S` applies **[audit: land-rewrite-signature-and-raw-extra]**.
- `cherry_pick`, `revert` and `squash` are built on the same primitives.

### 4.5 Path-set disjointness (shared by land, metadata rebase and merge fast paths)

`paths(base..tip)` = the union of the `changes` keys along the first-parent chain from `tip` back to (excluding) `base`, when `base` is on that chain. Otherwise it is `leafdiff(base.root, tip.root)` keys. This is a superset of the real difference, so it is conservative.

Two path sets **conflict** when some `a ∈ A`, `b ∈ B` satisfy any of: `a == b`; `casefold(a) == casefold(b)`; `a` is a directory prefix of `b` or the reverse (also compared under casefold) **[fix J1/J2: prefix and case conflicts]**. Implementation: insert B's casefolded paths and all of their ancestor directories into a trie, then test each A path, and each ancestor of it, against the trie.

### 4.6 Blame (`blame.rs`)

`blame(rev, path)` walks back from `rev` over changesets whose `changes` touch `path` (following `copy_from`). It line-diffs successive versions and assigns each line its origin. Results are memoised per `(changeset, path)` as the keyed derived type `blame.v1` (§5). On land branches the candidate list comes from the path index (§5.1). Output: git porcelain, incremental and default formats; `-L` ranges.

---

## 5. Derived data

```rust
#[async_trait]
pub trait Derived: Sized + Send + Sync {
    const NAME: &'static str;   // "gen", "gitcommit", "blame"
    const VERSION: u32;
    const EAGER: bool;          // must exist before any ref may point at the changeset
    async fn derive(e: &EngineCtx, cs: &Changeset, id: ChangesetId, parents: &[Self]) -> Result<Self>;
    fn encode(&self) -> Vec<u8>; fn decode(b: &[u8]) -> Result<Self>;
}
```

- `get::<D>(id)` finds the underived ancestors iteratively (topological order, no recursion), derives the oldest first, and memoises at `d/<NAME>.v<VERSION>/<id>` (and in SQL for the server types below). Derivation is pure, so concurrent derivers write identical bytes.
- **Keyed derived values [audit: 10-derived-trait-shape]:** values keyed by `(changeset, key)` that recurse along something other than parents use a second trait:

```rust
#[async_trait]
pub trait DerivedKeyed: Sized + Send + Sync {
    const NAME: &'static str; const VERSION: u32;
    type Key: Encode;
    async fn derive(e: &EngineCtx, cs: ChangesetId, key: &Self::Key) -> Result<Self>;
}
```

  Values are memoised at `d/<NAME>.v<V>/<blake3(cs ‖ encode(key))>`. `blame.v1` is `DerivedKeyed<Key = RepoPath>`; its derive recurses to the previous changeset that touched the path.
- **Derived data is lazy by default** (computed when first asked for, then written back). Only master-critical types are eager **[audit: user decision efficiency]**. Eager (blocks ref moves): `gen`, `skip`, `gitcommit`. The project graph is warmed asynchronously for every land-branch tip, so no request waits on it and `UpdateRefs` latency never includes manifest parsing **[audit: manifests-eager-terminology]**.

| Type | Eager | Value | Also written to |
|---|---|---|---|
| `gen.v1` | yes | `{ gen: u32 }` | server `cs_graph`; local `state.db` |
| `skip.v1` | yes | `{ ptrs: Vec<ChangesetId> }`: `ptrs[k]` = the first-parent ancestor `4^(k+1)` steps back, computed from the parent's pointers in O(log n) | – |
| `gitcommit.v1` | yes | `{ commit: GitOid }` | server `git_map`; local `state.db.git_map` |
| `manifests.v1` | no (`EAGER = false`; land-branch tips are derived asynchronously by `projects_warm`, §12A.5) | `{ set: [u8;32] }` (§12A.5) | – |
| `blame.v1` (keyed by path) | no | line runs `(start, len, origin ChangesetId, orig_line)` | – |
| `classify.v1` | no | §8.10 | – |
| `diffstat.v1` | no | `Vec<(path, added, removed)>` against `parents[0]`; feeds `CommitGraph` stats, visual file history and `Log.with_stats` (§20.4) | – |
| `tdocs.v1` (keyed by `TreeId`, per namespace) | no (`EAGER = false`; warmed for land tips, PR heads and drafts, §23.5) | roaring bitmap of the doc ordinals of every File/Exec entry in the subtree, plus `dups` counts (§23.2.3) | `d/` only |

- Mergeability (§20.4) is deliberately **not** a keyed derived type: it is the plain SQL table `mergeable`, one overwritten row per open head, so it cannot grow with trunk history.

- Content-keyed memos (`mset`, `parsed`, `pgraph`, `phash`, §12A.5) use `ObjKey::Derived` with the key given in §12A.5, like `blame.v1`.
- **Code-intelligence objects [audit: codeintel-folded]:** `tdocs.v1` (key `TreeId`) and the per-file objects `tags.v1` and `chunks.v1` (key `blake3(FileId ‖ lang ‖ version)`), plus the immutable index files `ciseg.v1` (tantivy segments), `civec.v1` (vector segments) and `ciidx.v1` (indexed bitmaps) are `ObjKey::Derived` objects under `d/`: evictable, recomputable and per namespace (§23.2.1, §23.2.4). They are never copied across namespaces (§3.3).
- **Named future types** (each is one impl plus registration, with no client change): `pathlog` (a per-path changeset list for non-land refs), `copies` (copy detection), `pathsearch`.
- **Warm-ref rule:** `UpdateRefs`, local commits and imports derive every eager type for new changesets before a ref moves. Readers never wait.
- On ingest the server also writes the **git map** for every new tree, file and changeset: `(git oid, kind) -> asc id`. Trees and files come from `Entry.git`. Changesets come from `gitcommit`.
- Adding a type means one `impl Derived` (or `DerivedKeyed`) plus registration. The write path never changes.

### 5.1 Mainline and path index (server; land branches) [audit: 30-ancestry-path-indexes]

```sql
mainline(ns BLOB, ref TEXT, pos BIGINT, cs BLOB, PRIMARY KEY(ns, ref, pos))  UNIQUE(ns, ref, cs)
path_changes(ns BLOB, ref TEXT, path_key BLOB /*first 16 bytes of blake3(path)*/, pos BIGINT,
             PRIMARY KEY(ns, ref, path_key, pos))
```

- **Maintenance:** for every ref in `repos.land_branches` (default `[default_branch]`), inside the §8.3 step 8 transaction:
  - the new first-parent changesets between the old and new tip are appended at consecutive positions;
  - each gets one `path_changes` row per changed path and per ancestor directory of a changed path (root included), deduplicated. Cost is O(changed paths × depth);
  - a forced non-fast-forward update deletes rows with `pos >` the new tip's position, then appends.
- **Normative uses:**
  - `IsAncestor(a, t)` with `t` a land-branch tip and `a` on its mainline compares positions (O(1));
  - `Log(ref, paths, first_parent)` on a land branch is a descending range scan of `path_changes`;
  - `LastCommits(dir, names)` is one batched `MAX(pos)` query;
  - the blame candidate list is the `path_changes` rows for the path, plus `copy_from` hops;
  - `FindLanding` uses positions (§8.9).
- Off-mainline ancestry uses the generation-pruned walk with skip pointers (§4.3). `asc_graph_walk_nodes` records walk sizes.
- **Pagination:** `LogRequest` gains `page_token`, and `LogResponse` gains `next_page_token`. Servers MUST paginate by cursor (a position or generation), never by scanning skipped entries. `skip` remains only for git's `--skip` and is translated to a position offset on land branches.
- **Rule and test:** no server RPC on a land branch reads more than O(result + log n) changesets. `EngineCtx` counts changeset reads. Tests over a 100k-commit synthetic history assert this bound for `Log` with paths, `LastCommits`, `IsAncestor`, blame candidate lookup and `FindLanding`. Postgres `EXPLAIN` tests assert no sequential scan on `ref_log`, `mainline`, `path_changes`, `git_map` or `land_map`.

---

## 6. Repositories, remotes and modes

### 6.1 Repository kinds on a machine

Every repository a machine knows has a row in `state.db.repos` (`id`, `name`, `ns`, `remote`, `default_branch`, `windows_safe`). Local branches and tags always live in `state.db.refs` (as git keeps local refs locally). Remote-tracking refs are `refs/remotes/<remote>/*` in the same table.

**Branch scope [audit: branch-scope-model]:** local branches are machine-local rows (one `state.db`, shared by that machine's workspaces) and reach the server only through `push`. In a branchless repo (the default, §21) the only global refs are the `shared_refs` (the default branch, release branches, `green/*`, tags) and PR heads; a pushed non-shared branch becomes the pusher's owner-only user ref (§21.5.1), and `refs/heads/agent/*` user refs keep their TTL (§8.8). Drafts are not refs: while the client is online the daemon uploads new changesets and their `ChangeMeta` to the pusher's cloud workspace (§21.3), where repo writers see them by change id, so `push` and `submit` find nothing left to upload. Local-only repos, bucket and git remotes, mirror-mode servers, public repos and repos with branchless or cloud off keep drafts in `store/` until `push` (§21.3.3). In a branch-mode repo the earlier model holds: pushed branches are global refs.

| Mode | Authoritative objects | Authoritative remote refs | How history ops run |
|---|---|---|---|
| **local-only** (`asc init`) | `store/` | none (local refs are the only refs) | local engine |
| **bucket remote** (`s3://`, `file://`) | the bucket | local (single-writer, the v1 default) or `BucketRefs` (shared, once U30 ships) | local engine over `Tiered<StoreSet, BucketObjects>` |
| **git remote** (`git+https://`, or Forgejo without asc) | the git server | the git server | local engine; import on fetch; export + `git push` on push |
| **server remote** (`asc+https://`, or Forgejo with asc discovered) | the server's store | the server's SQL | local engine for local work; server RPCs for `Diff --remote`, blame, long-range log and CI (`asc.serverHistory=auto`) |

### 6.2 Remote URLs and discovery

| URL | Meaning |
|---|---|
| `https://host/owner/repo[.git]` | Discover. Use `remote.<name>.ascServer` if set. Otherwise `GET https://host/.well-known/asc`, which returns `{ "server": "https://asc.host:443", "objects": ["https://asc-cache.<region>:7890", …], "seeds": "https://…", "repos": "*", "mode": {"<owner>/<repo>": "mirror"\|"primary"}, "min_client": "x.y.z" }` (`objects` and `seeds` optional, §7, §12.3). If that is 404, use git-remote mode. |
| `asc+https://host[:port]/owner/repo` | server remote |
| `git+https://…`, `git+ssh://…`, `git@host:path` | git-remote mode, forced |
| `s3://bucket/prefix?endpoint=<url>&region=<r>&path_style=true` | bucket remote. Credentials from `AWS_*` env, or `git credential fill` with `protocol=s3`, `host=<bucket>`. |
| `file:///abs/path` or an absolute path to a bucket-layout dir | bucket remote on the local filesystem |

Before the Forgejo fork serves `/.well-known/asc`, ASC's reverse proxy routes that path to `asc serve`.

### 6.3 Fetch, push and clone (`asc-engine::sync`)

Negotiation follows Scion. There are no packfiles and no negotiation round trips:

- **fetch (server remote) [audit: 2/12-history-fetch]:** list remote refs. For changed refs, call `Log(include = new tips, exclude = current remote-tracking tips)`. Only ref tips are sent, never a set of objects the client has. Write `cs_gen` rows (`gen`, `parents`) straight from the streamed `LogEntry`s; the client trusts its remote's `gen` as it trusts ref values, and `asc fsck` re-derives it. Then fetch the changeset bodies with pipelined `GetObjects` batches of 256 ids (8 streams). Trees and files are not fetched (`--prefetch <pathspec>` warms them, tree first, then blobs). Update remote-tracking refs. Write `gitcommit` locally.
  - **History window [audit: clone-history-o-history]:** changeset bodies and `cs_gen` rows are fetched only for the `asc.fetch.historyWindow` generations (default 10,000) below each fetched tip, via `Log(limit)`. The oldest fetched changesets whose parents are not local are recorded in `state.db.history_boundary(repo_id, cs)`. A local walk (log, merge-base, ahead/behind, blame, describe) that reaches a boundary continues through the server's `Log`/`MergeBase`/`IsAncestor` (passing the boundary ids) when online (§4.3). Offline it fails with the §6.4 offline error. Output is identical to a full history, and `rev-parse --is-shallow-repository` prints `false`. `asc fetch --deepen=<n>`/`--unshallow` and `asc prefetch --history N` widen the window. Bucket remotes keep the full walk.
- **fetch (bucket remote, no server):** a breadth-first walk that requests every unknown parent of the current frontier in one batch, so it costs one round trip per generation level, not per changeset. `d/gen.v1`, `d/skip.v1` and `d/gitcommit.v1` values are read from the bucket when present.
- **Want-only, tree then blob [audit: user decision efficiency]:** every object fetch names explicit ids. A checkout or prefetch requests the root tree, then missing subtrees level by level, then blobs. Objects travel as stored bytes (§3.2) in batches, so any tier can answer and an interrupted transfer resumes by asking again.
- **push:**
  1. compute the closure of new local objects: changesets not on the remote; for each, its new trees (walk `changes` spines), files (and chunk lists, chunks, tags);
  2. `HasObjects` in batches of 4,096;
  3. upload missing objects in dependency order (files and chunks, trees bottom-up, then changesets) with `PutObjects`, using up to 8 concurrent streams of ≤ 16 MiB batches;
  4. update refs (`UpdateRefs` with `expect = Value(remote-tracking value)`, or `Any` with `--force`, or the lease given by `--force-with-lease`);
  5. on success, demote pushed objects from `store/` to `cache/`.

  A retry of an interrupted push re-runs `HasObjects` and so resumes. Background uploads are gated and bounded by §21.3 (daemon only, never on public repos, after the pre-upload guard of §21.3.7), which replaces the earlier "nothing is uploaded before `asc push`" rule; `push` and `submit` drain any queued entries in their closure first and find the rest already on the server (§21.3.1). A push that would upload a changeset carrying `extra["git.commit"]` (an imported raw commit, §8.4) fails before any upload with `fatal: asc: import this history on the server (asc serve admin import)` **[audit: client-raw-git-extras]**.
- **clone:** register the repo; if the discovery document names a seed (§12.3), unpack it into `cache/`; fetch refs, the head changeset and its root tree, and the history metadata of the fetched tips as `fetch` does (within `asc.fetch.historyWindow`; ASC's 5.5k changesets fit, so ASC history stays fully offline), so `log`, `merge-base` and ahead/behind work offline inside the window; create a workspace (§9). File and tree downloads are O(1) in repository size. Clone cost is O(window + root tree), independent of history length. ASC's metadata is about 3 MiB compressed, and the `< 5 MiB` clone target includes it.
- **Git-remote mode:** fetch = real `git fetch` (§11.4 `RealGit`; credential helpers apply) into the private bare repo `repos/<id>/git-remote.git` (full objects, about 0.4 GB for ASC, once per machine), then an incremental import of new commits (§14.1) into `cache/`, which is evictable because it is rebuildable from git. Push = export new changesets into that bare repo (§14.2), then `git push <url> <sha>:<ref> --force-with-lease=<ref>:<expected sha>`, then update remote-tracking refs. The bare repo is never a workspace gitdir. `asc clone <url> --reference <path-to-existing-.git>` first runs `git fetch <path>` into it (copying reachable objects only, with no alternates link), so the old checkout can be deleted afterwards.
- **Mirror-mode server (Stage 1):** fetch and clone use the server. Push uses the git-remote push path to Forgejo, and the server imports the result (§8.6). `UpdateRefs` from clients is rejected with `FAILED_PRECONDITION: mirror mode`.

### 6.4 Offline behaviour

These work offline: `status`, `add`, `commit`, `diff` of the working copy, `stash`, `branch`, `tag`, `switch` between cached revisions, `merge`/`rebase`/`cherry-pick` when the content is cached, `log`/`blame` over cached history inside the history window, and reads of cached files. Everything else, including history beyond the window (§6.3), fails fast with `fatal: asc: offline (<remote> unreachable): <operation> needs <RPC>` (exit 128, §11.6). A virtual read of uncached content fails with `STATUS_FILE_IS_OFFLINE` (ProjFS) or `EIO` (FUSE), and the daemon log names the path. `asc prefetch <pathspec> [--history N]` warms the cache before going offline. Commits made offline join the upload queue and never block or print anything; the daemon uploads them when the server is reachable again (§21.3.3).

### 6.5 Default branch [audit: master-default, default-branch-master]

The default branch name is `master` everywhere asc chooses one. asc never chooses `main`.

- `asc init` (and `asc init --store`) uses `-b`, else `init.defaultBranch` from git config, else `master`. asc never prints git's default-branch hint.
- `asc clone` uses the remote's symbolic HEAD, or `master` when there is none. Fetch and import keep existing branch names unchanged.
- `RepoAdmin.init` with an empty `default_branch` uses `master`; `land_branches` defaults to `[default_branch]`.
- The shim adds `-c init.defaultBranch=master` to real git `init` and `clone` when the user has not configured `init.defaultBranch` (§11.4). This matters because git 3.0 changes its built-in default to `main`.
- `asc affected` defaults `--base` to `refs/remotes/origin/HEAD`, else `refs/remotes/origin/<default branch>`, else the local default branch, and compares at the merge base (§12A.9). `asc ci changed|affected` take `--base` explicitly. `asc ci warm --ref` defaults to `<default_branch>`.
- The Forgejo fork and the deploy bundle set `[repository] DEFAULT_BRANCH = master` in `app.ini` (tests and `deploy/`).
- Every fixture, test, compat `HOME` config (`init.defaultBranch=master`) and doc example uses `master`.
- **CI lint [audit: lint-main-fails-own-docs, no-main-lint-self-fails, no-main-lint-misses-common-forms]:** `build/lint/no-main.sh` fails on branch-name uses of `main` only: `rg -n -P '(refs/(heads|remotes/[^/\s]+)/main\b|\b(origin|upstream)[/ ]main\b|\bpush\b.*\S:main\b|(\s-[bBcCmM]|--branch|--initial-branch|--base|--head|--ref|--onto|--rev)[ =]main\b|(?i:default_?branch)["\x27]?\s*[:=]\s*["\x27]?main\b|\b(ref|base|branch|branches)\s*:\s*\[?\s*["\x27]?main\b|^\s*-\s*["\x27]?main["\x27]?\s*(#.*)?$|\bmain\.\.|\.\.\.?main\b|\b(switch|checkout|merge|rebase|pull|fetch|reset|diff|log|branch)\s+(-\S+\s+)*main\b|\b(switch|checkout)\s+-[bBcC]\s+\S+\s+main\b|\bworktree\s+add\s+(-\S+\s+)*\S+\s+main\b)'` over `docs/*.md README.md crates tests deploy actions .forgejo bench packaging proto build --glob '!build/lint/fixtures/**'`. <!-- lint:allow-main -->
  It matches the push, pull, merge, rebase, `checkout -b`, `branch -M`, `HEAD:main`, `--base[=]main`, Actions `ref: main`, `default_branch`/`defaultBranch`/`DEFAULT_BRANCH` and YAML block-list (`- main`) forms. So `main.rs`, `fn main`, `#[tokio::main]`, `image:main` and prose such as "never `main`" or "the main worktree" pass. `bench/gen` and every script that runs real git pass `-c init.defaultBranch=master`. A line carrying the marker `lint:allow-main` is exempt (in Markdown a trailing `<!-- lint:allow-main -->`, in code a `// lint:allow-main` or `# lint:allow-main` comment); only lines that quote a forbidden form, such as the lint's own self-test fixture, carry it. The `docs/designs/*` headers state that `main` there is superseded by this section. <!-- lint:allow-main -->

---

## 7. RPC surface (`proto/asc/v1/repo.proto`, `workspace.proto`, `changes.proto`, `cloud.proto`, `options.proto`, `cli.proto`)

gRPC over HTTP/2 (`tonic` 0.14), TLS on the network, `interprocess` local sockets for the daemon. The Go fork generates stubs from the same files (`buf generate`). Objects travel as stored bytes (codec byte plus data).

**Lint [audit: proto-fails-buf-standard-lint]:** `proto/buf.yaml` uses buf STANDARD except `SERVICE_SUFFIX`, `ENUM_ZERO_VALUE_SUFFIX`, `RPC_REQUEST_STANDARD_NAME`, `RPC_RESPONSE_STANDARD_NAME` and `RPC_REQUEST_RESPONSE_UNIQUE`. Zero enum values are meaningful defaults (for example `ID_MODE_ASC`), and the Workspace plugin RPCs deliberately reuse the `Repo` response messages.

**Stacked changes:** the `Changes` service (`proto/asc/v1/changes.proto`, same port) and the additive `repo.proto`/`workspace.proto` fields for stacks and history insight are specified in §20.5 and §20.6.

**Commit cloud:** the `Cloud` service (`proto/asc/v1/cloud.proto`, same port) and the additive `repo.proto` fields (`Version.features += "cloud.v1", "branchless.v1"`; `RepoConfigResponse += branchless, cloud, shared_refs, visibility`; `WatchEvent` kind `KIND_CLOUD`) are specified in §21.3.5 [audit: branchless-folded].

**MCP and the JSON profile (§22) [audit: mcp-folded]:** `options.proto` holds the `(asc.v1.json)` field option and `cli.proto` the messages of asc-only verbs (`OpResult`, `OpLogResponse`, the `*Args` messages, `AscHandles`, `WorktreeAddResult`). `asc serve` also mounts `POST /mcp` (§22.2). CLI `--json` prints the versioned **asc JSON profile** `json.profile.v1` (§22.4.2): snake_case field names, lowercase hex ids (40 = git SHA, 64 = asc id), change ids as 32 letters `k`–`z`, paths and text as UTF-8 (otherwise `{"b64": …}`), enums as lowercase names without the type prefix, and 64-bit integers as JSON numbers. It differs from standard proto3 JSON, which `asc api` keeps (below). The two never mix, and `asc_rpc::json` is the profile's only codec.

**Code intelligence (§23) [audit: codeintel-folded]:** the `CodeIntel` service (`proto/asc/v1/codeintel.proto`, same port: `Search` (streaming), `Symbols`, `Semantic`, `IndexStatus`; full messages in §23.4.1) and the same four RPCs on `workspace.proto` (workspace scope, IPC only). `Version.features += "search.v1"` when the service is enabled. A cache-role server serves none of them.

**IDE bridge (§24) [audit: ide-plugins-folded]:** `proto/asc/v1/bridge.proto` (`service Bridge`: `Hello`, `Run`, `Token`, `StoreToken`, `Events`; `BridgeFrame`, `Call`, `End`, `RunRequest` with no stdin field; full text in §24.2) is the transport of ASC's IDE plugins. `asc api --stdio` carries length-prefixed `BridgeFrame` messages and routes each call to the daemon IPC (`Workspace`), the asc server (`Repo`, `Changes`, `Projects`, `Cloud`, `CodeIntel`) or the in-process engine, with the same `LocalBackend` routing as `asc mcp`. The plugins therefore reach every RPC without a Node or JVM client for the named pipe. `Bridge.Run` runs an asc verb with `--json` against a per-verb flag schema and returns the typed `OpResult` of `cli.proto`. `HelloResponse.features` joins the bridge flags (`plugin.v1`, `json-verbs.v1`, `reorder-todo.v1`, `split-hunks.v1`) with `Repo.Version.features`; gating uses features, never version comparison.

**Common metadata:** `authorization: Bearer <token>`; `x-asc-client: asc/<semver> <os>-<arch>`; `x-asc-op-id: <uuidv7>` on every mutating call (idempotency, §8.3); W3C `traceparent`.

**Errors** use gRPC status codes with an `asc.v1.ErrorDetail { string code; repeated string paths; bytes actual; repeated Conflict conflicts; }` detail. `code` values: `REF_CONFLICT`, `NOT_FAST_FORWARD`, `LAND_CONFLICT`, `LAND_NEEDS_CLIENT_REBASE`, `MISSING_OBJECTS`, `INVALID_OBJECT`, `HOOK_REJECTED`, `MIRROR_MODE`, `CLIENT_TOO_OLD`, `PATH_POLICY`, `PROJECTS_CONFIG`, `RATE_LIMITED`, and for stacks (§20.5) `CHANGE_DIVERGED`, `CHANGE_FROZEN`, `CHANGE_NOT_ALLOWED`, `LAND_NEEDS_CONTENT_MERGE`, and for branchless (§21.3.5) `VERSION_CONFLICT`, `CLOUD_HEADS_LIMIT`, `CLOUD_WORKSPACES_LIMIT`, `USER_REFS_LIMIT`, `QUOTA_EXCEEDED`, `REF_NOT_SHARED`.

**Public API contract [audit: 31-api-standardisation]:** `asc.v1` is the public, stable API for CI, tools, agents and ASC's IDE plugins. `docs/api.md` is generated from the proto comments (buf). For each RPC it lists the required permission (read, write, admin or internal), idempotency, pagination and rate-limit class (§16.3). Every list RPC paginates with an opaque `page_token`/`next_page_token`. `asc api <Rpc> [--json <request>|-]` calls any `Repo` or `Projects` RPC with the user's credentials and prints JSON (the standard proto3 JSON mapping, not the §22.4.2 profile), so scripts need no stubs. The proto is forge-neutral: no Forgejo types appear in `repo.proto`, so another forge or a git server can implement the same service.

**Object tier contract [audit: 17-tier-contract, user decision efficiency]:**
- The tier RPCs are `Version`, `HasObjects` and `GetObjects`. Any process that implements them for a namespace is a tier: the primary `asc serve`, `asc serve --role cache`, and the CI `Tiered` stack. Clients make no assumption about what is upstream.
- **Discovery (v1 clients MUST honour it):** when `/.well-known/asc` lists `objects`, the client sends `HasObjects`/`GetObjects` to the first reachable URL in the list and all other RPCs to `server`. `remote.<name>.ascObjects` overrides the list.
- **Cache role (built in v1):** `asc serve --role cache --upstream <url>`. It has no SQL. It keeps a memory and disk LRU of stored bytes (§3.2 `get_stored`) and serves only the tier RPCs; other RPCs return `UNIMPLEMENTED`. Authorisation is forwarded upstream per `(sha256(token), repo)` and cached as in §15.1. Concurrent misses are coalesced into one upstream `GetObjects` (§16.3).

```proto
syntax = "proto3";
package asc.v1;
option go_package = "forgejo.org/modules/ascproto/ascv1";

enum IdMode { ID_MODE_ASC = 0; ID_MODE_GIT = 1; }   // GIT: changeset/tree/file ids are git oids (20 bytes) in and out
message RepoRef { string name = 1; IdMode id_mode = 2; }   // name = "owner/repo"

enum ObjKind { OBJ_KIND_UNSPECIFIED = 0; OBJ_KIND_FILE = 1; OBJ_KIND_CHUNK_LIST = 2; OBJ_KIND_CHUNK = 3;
               OBJ_KIND_TREE = 4; OBJ_KIND_CHANGESET = 5; OBJ_KIND_TAG = 6; OBJ_KIND_GIT_TREE_OVERRIDE = 7;
               reserved 8; OBJ_KIND_CHANGE_META = 9; }   // 8 was SHA256_ALIAS (removed with LFS); 9 = ChangeMeta (§20.2)
message ObjKey { ObjKind kind = 1; bytes id = 2; }          // 32 bytes (20 for GIT_TREE_OVERRIDE)
message Object { ObjKey key = 1; bytes data = 2; }          // stored bytes: codec ‖ data

message RefTarget { oneof t { bytes changeset = 1; bytes tag = 2; } }
message Ref { string name = 1; RefTarget target = 2; uint64 version = 3; int64 expires_at = 4; }
message Signature { bytes name = 1; bytes email = 2; int64 time = 3; int32 tz_min = 4; }

service Repo {
  rpc Version(VersionRequest) returns (VersionResponse);
  rpc HasObjects(HasObjectsRequest) returns (HasObjectsResponse);
  rpc GetObjects(stream GetObjectsRequest) returns (stream GetObjectsResponse);
  rpc PutObjects(stream PutObjectsRequest) returns (PutObjectsResponse);
  rpc ListRefs(ListRefsRequest) returns (ListRefsResponse);
  rpc UpdateRefs(UpdateRefsRequest) returns (UpdateRefsResponse);
  rpc Resolve(ResolveRequest) returns (ResolveResponse);
  rpc GitMap(GitMapRequest) returns (GitMapResponse);
  rpc Log(LogRequest) returns (stream LogResponse);
  rpc MergeBase(MergeBaseRequest) returns (MergeBaseResponse);
  rpc IsAncestor(IsAncestorRequest) returns (IsAncestorResponse);
  rpc Diff(DiffRequest) returns (stream DiffResponse);
  rpc Blame(BlameRequest) returns (stream BlameResponse);
  rpc LastCommits(LastCommitsRequest) returns (LastCommitsResponse);
  rpc ReadChangeset(ReadChangesetRequest) returns (ReadChangesetResponse);
  rpc ReadTree(ReadTreeRequest) returns (stream ReadTreeResponse);
  rpc ReadFile(ReadFileRequest) returns (stream ReadFileResponse);
  rpc Stat(StatRequest) returns (StatResponse);
  rpc CheckAttr(CheckAttrRequest) returns (CheckAttrResponse);
  rpc Grep(GrepRequest) returns (stream GrepResponse);
  rpc Archive(ArchiveRequest) returns (stream ArchiveResponse);
  rpc Commit(CommitRequest) returns (CommitResponse);
  rpc Merge(MergeRequest) returns (MergeResponse);
  rpc Apply(ApplyRequest) returns (ApplyResponse);
  rpc CopyClosure(CopyClosureRequest) returns (CopyClosureResponse);
  rpc RepoAdmin(RepoAdminRequest) returns (RepoAdminResponse);
  rpc ImportGitPush(ImportGitPushRequest) returns (ImportGitPushResponse);   // Forgejo internal token only
  rpc Ingest(IngestRequest) returns (IngestResponse);                        // §8.7
  rpc FindLanding(FindLandingRequest) returns (FindLandingResponse);         // §8.9
  rpc Classify(ClassifyRequest) returns (ClassifyResponse);                  // §8.10
  rpc Rebase(RebaseRequest) returns (RebaseResponse);                        // §8.11
}
// service Projects (ProjectGraph, Affected, ProjectHashes) is in proto/asc/v1/projects.proto (§12A.8), on the same port.

message VersionRequest {}
message VersionResponse { string server_version = 1; string min_client = 2; repeated string features = 3; }

message HasObjectsRequest { RepoRef repo = 1; repeated ObjKey keys = 2; }           // ≤ 4096 keys
message HasObjectsResponse { bytes present = 1; }                                   // bitmap, LSB-first per byte
message GetObjectsRequest { RepoRef repo = 1; repeated ObjKey keys = 2; }           // ≤ 256 keys per message
message GetObjectsResponse { repeated Object objects = 1; repeated ObjKey missing = 2; } // ≤ 16 MiB per message
message PutObjectsRequest { RepoRef repo = 1; repeated Object objects = 2; }
message PutObjectsResponse { uint64 stored = 1; uint64 already_present = 2; }

message ListRefsRequest { RepoRef repo = 1; string prefix = 2; bytes contains = 3; string after = 4; uint32 limit = 5; }
message ListRefsResponse { repeated Ref refs = 1; string next = 2; string head = 3; }   // after/next = opaque page token; head = symbolic HEAD
message RefUpdate { string name = 1;
  oneof expect { bool any = 2; bool absent = 3; RefTarget value = 4; }
  RefTarget new = 5;  bool delete = 6; }
enum UpdateMode { UPDATE_MODE_FAST_FORWARD = 0; UPDATE_MODE_FORCE = 1; UPDATE_MODE_LAND = 2; }
enum HookMode { HOOK_MODE_RUN = 0; HOOK_MODE_SKIP_PRE = 1; }                        // SKIP_PRE: internal token only
message UpdateRefsRequest { RepoRef repo = 1; repeated RefUpdate updates = 2; UpdateMode mode = 3;
  repeated string push_options = 4; HookMode hooks = 5; int64 ttl_seconds = 6; }
message Rewrite { bytes old = 1; bytes new = 2; }
message RefResult { string name = 1; RefTarget old = 2; RefTarget new = 3; }
message UpdateRefsResponse { repeated RefResult results = 1; repeated Rewrite rewritten = 2; string message = 3; }

message ResolveRequest { RepoRef repo = 1; repeated string revspecs = 2; }
message ResolveResponse { repeated bytes ids = 1; }                                  // empty bytes = not found
message GitMapRequest { RepoRef repo = 1; repeated bytes asc = 2; repeated bytes git = 3; }
message GitMapResponse { repeated bytes asc_to_git = 1; repeated bytes git_to_asc = 2; }

message LogRequest { RepoRef repo = 1; repeated string include = 2; repeated string exclude = 3;
  repeated string paths = 4; bool first_parent = 5; bool follow = 6; uint32 skip = 7; uint32 limit = 8;
  string author = 9; string grep = 10; int64 since = 11; int64 until = 12; bool count_only = 13; string page_token = 14; }
message LogEntry { bytes id = 1; bytes git = 2; repeated bytes parents = 3; Signature author = 4;
  Signature committer = 5; bytes message = 6; uint32 gen = 7; }
message LogResponse { repeated LogEntry entries = 1; uint64 count = 2; string next_page_token = 3; }
message MergeBaseRequest { RepoRef repo = 1; bytes a = 2; repeated bytes b = 3; bool all = 4; }
message MergeBaseResponse { repeated bytes bases = 1; }
message IsAncestorRequest { RepoRef repo = 1; bytes ancestor = 2; bytes descendant = 3; }
message IsAncestorResponse { bool result = 1; }

enum DiffFormat { DIFF_FORMAT_NAMES = 0; DIFF_FORMAT_STATUS = 1; DIFF_FORMAT_STAT = 2; DIFF_FORMAT_NUMSTAT = 3;
                  DIFF_FORMAT_PATCH = 4; DIFF_FORMAT_RAW = 5; }
message DiffRequest { RepoRef repo = 1; string base = 2; string head = 3; bool merge_base = 4;
  repeated string paths = 5; DiffFormat format = 6; bool renames = 7; uint32 context = 8; bool binary = 9;
  bool null_terminated = 10; uint32 max_files = 11; }
message PathChange { bytes path = 1; string status = 2; bytes old_path = 3; uint32 old_mode = 4; uint32 new_mode = 5;
  bytes old_git = 6; bytes new_git = 7; bytes old_id = 8; bytes new_id = 9; uint64 old_size = 10; uint64 new_size = 11;
  uint32 similarity = 12; }
message DiffResponse { repeated PathChange changes = 1; bytes text = 2; bool truncated = 3; }  // text = git-format output

message BlameRequest { RepoRef repo = 1; string rev = 2; bytes path = 3; uint32 start_line = 4; uint32 end_line = 5;
  bool reverse = 6; }
message BlameHunk { bytes origin = 1; bytes orig_path = 2; uint32 orig_start = 3; uint32 start = 4; uint32 len = 5; }
message BlameResponse { repeated BlameHunk hunks = 1; repeated LogEntry commits = 2; }
message LastCommitsRequest { RepoRef repo = 1; string rev = 2; bytes dir = 3; repeated bytes names = 4; }
message LastCommitsResponse { map<string, LogEntry> by_name = 1; }

message ReadChangesetRequest { RepoRef repo = 1; string rev = 2; }
message ReadChangesetResponse { LogEntry header = 1; bytes root_tree = 2; bytes root_git = 3; repeated PathChange changes = 4;
  map<string, bytes> extra = 5; bytes raw_git_commit = 6; }
message ReadTreeRequest { RepoRef repo = 1; string rev = 2; bytes path = 3; bool recursive = 4; uint32 limit = 5;
  bytes after = 6; }
message TreeEntryInfo { bytes name = 1; bytes path = 2; uint32 mode = 3; bytes id = 4; bytes git = 5; uint64 size = 6;
  reserved 7, 8; }   // were lfs, lfs_size
message ReadTreeResponse { repeated TreeEntryInfo entries = 1; bytes tree_id = 2; bytes tree_git = 3; }
message ReadFileRequest { RepoRef repo = 1; oneof at { string rev_path = 2; bytes file_id = 3; } uint64 offset = 4;
  uint64 length = 5; reserved 6; }   // was resolve_lfs
message ReadFileResponse { bytes data = 1; uint64 total_size = 2; }
message StatRequest { RepoRef repo = 1; string rev = 2; repeated bytes paths = 3; repeated ObjKey objects = 4; }
message StatResponse { repeated TreeEntryInfo entries = 1; repeated bool exists = 2; }
message CheckAttrRequest { RepoRef repo = 1; string rev = 2; repeated bytes paths = 3; repeated string names = 4; }
message AttrValues { map<string, string> values = 1; }
message CheckAttrResponse { repeated AttrValues per_path = 1; }
message GrepRequest { RepoRef repo = 1; string rev = 2; string pattern = 3; bool fixed = 4; bool ignore_case = 5;
  repeated string paths = 6; uint32 max_results = 7; }
message GrepMatch { bytes path = 1; uint32 line = 2; bytes text = 3; }
message GrepResponse { repeated GrepMatch matches = 1; }
message ArchiveRequest { RepoRef repo = 1; string rev = 2; string format = 3; string prefix = 4; repeated string paths = 5; }
message ArchiveResponse { bytes data = 1; }

message FileOp { bytes path = 1;
  oneof op { bytes put_content = 2; bytes put_file_id = 3; bool delete = 4; bytes move_from = 5; }
  uint32 mode = 6; }
message CommitRequest { RepoRef repo = 1; string parent = 2; repeated FileOp ops = 3; bytes message = 4;
  Signature author = 5; Signature committer = 6; string update_ref = 7; RefTarget expect = 8; bool sign = 9; }
message CommitResponse { bytes id = 1; bytes git = 2; }
enum MergeStyle { MERGE_STYLE_MERGE = 0; MERGE_STYLE_SQUASH = 1; MERGE_STYLE_REBASE = 2; MERGE_STYLE_REBASE_MERGE = 3;
  MERGE_STYLE_FAST_FORWARD_ONLY = 4; }
message MergeRequest { RepoRef repo = 1; string base = 2; string head = 3; RepoRef head_repo = 4; MergeStyle style = 5;
  bytes message = 6; Signature author = 7; Signature committer = 8; bool dry_run = 9; string update_ref = 10;
  repeated string protected_globs = 11; bool sign = 12; }
enum ConflictKind { CONFLICT_KIND_CONTENT = 0; CONFLICT_KIND_ADD_ADD = 1; CONFLICT_KIND_MODIFY_DELETE = 2;
  CONFLICT_KIND_FILE_DIR = 3; CONFLICT_KIND_CASE_FOLD = 4; CONFLICT_KIND_BINARY = 5; }
message Conflict { bytes changeset = 1; bytes path = 2; ConflictKind kind = 3; bytes base_id = 4; bytes ours_id = 5;
  bytes theirs_id = 6; bytes marked_id = 7; }                 // marked_id: FileId of the diff3-style marked result (§8.11)
message MergeResponse { bytes result = 1; repeated bytes conflicts = 2; repeated bytes changed_protected = 3;
  bool is_empty = 4; repeated Rewrite rewritten = 5; repeated Conflict conflict_details = 6; }
enum ApplyMode { APPLY_MODE_CHERRY_PICK = 0; APPLY_MODE_REVERT = 1; APPLY_MODE_PATCH = 2; }
message ApplyRequest { RepoRef repo = 1; string onto = 2; string changeset = 3; bytes patch = 4; ApplyMode mode = 5;
  bytes message = 6; Signature author = 7; Signature committer = 8; string update_ref = 9; }
message ApplyResponse { bytes result = 1; repeated bytes conflicts = 2; repeated Conflict conflict_details = 3; }
message CopyClosureRequest { RepoRef from = 1; RepoRef to = 2; repeated bytes changesets = 3; }
message CopyClosureResponse { uint64 copied = 1; }

message RepoAdminRequest { RepoRef repo = 1;
  oneof op { InitOp init = 2; ForkOp fork = 3; bool delete = 4; string rename_to = 5; bool size = 6; bool verify = 7; } }
message InitOp { string default_branch = 1; bool windows_safe = 2; string mode = 3; /* "primary"|"mirror" */
  string mirror_url = 4; repeated string land_branches = 5; }
message ForkOp { RepoRef from = 1; }
message RepoAdminResponse { uint64 logical_size = 1; uint64 unique_size = 2; string report = 3; }

message GitRefUpdate { string name = 1; bytes old_git = 2; bytes new_git = 3; }
message ImportGitPushRequest { RepoRef repo = 1; string git_dir = 2; string object_dir = 3;
  repeated string alternate_dirs = 4; repeated GitRefUpdate updates = 5; int64 pusher_id = 6; }
message ImportGitPushResponse { repeated RefResult results = 1; }

message IngestRequest { RepoRef repo = 1; repeated bytes changesets = 2; }
message IngestResponse { repeated bytes ingested = 1; }
message FindLandingRequest { RepoRef repo = 1; bytes changeset = 2; string ref = 3; }       // asc or git id
message FindLandingResponse { bool found = 1; bytes landed_as = 2; uint64 ref_log_seq = 3; int64 at = 4;
  string actor = 5; string via = 6; }
enum ChangeClass { CHANGE_CLASS_RENAME_ONLY = 0; CHANGE_CLASS_MODE_ONLY = 1; CHANGE_CLASS_WHITESPACE_ONLY = 2;
  CHANGE_CLASS_COMMENT_ONLY = 3; CHANGE_CLASS_DOCS = 4; CHANGE_CLASS_GENERATED = 5; CHANGE_CLASS_LOCKFILE = 6;
  CHANGE_CLASS_TEST = 7; CHANGE_CLASS_LOGIC = 8; }
message ClassifyRequest { RepoRef repo = 1; string base = 2; string head = 3; bool merge_base = 4; }
message FileClass { bytes path = 1; ChangeClass class = 2; }
message ClassifyResponse { repeated FileClass files = 1; ChangeClass overall = 2; }
message Resolution { bytes changeset = 1; bytes path = 2; FileOp op = 3; }
message RebaseRequest { RepoRef repo = 1; RepoRef head_repo = 2; string base = 3; string head = 4; string onto = 5;
  repeated Resolution resolutions = 6; bool dry_run = 7; string update_ref = 8; RefTarget expect = 9; UpdateMode mode = 10; }
message RebaseResponse { bytes result = 1; repeated Rewrite rewritten = 2; repeated Conflict conflicts = 3; }
```

```proto
// workspace.proto — daemon IPC only (named pipe / Unix socket, current user only)
// The plugin RPCs (Status … Watch, ReadFile) are the stable, documented API for ASC's IDE plugins [audit: user decision No-IDE, superseded in part by ide-plugins-folded].
service Workspace {
  rpc Status(WsStatusRequest) returns (WsStatusResponse);       // ws_id → staged, unstaged, untracked, conflicts, ahead/behind, in-progress op
  rpc Diff(WsDiffRequest) returns (stream DiffResponse);        // worktree/index/rev pairs, same formats as Repo.Diff
  rpc Log(WsLogRequest) returns (stream LogResponse);           // local engine, or the server per asc.serverHistory
  rpc Blame(WsBlameRequest) returns (stream BlameResponse);     // includes uncommitted lines
  rpc Affected(WsAffectedRequest) returns (WsAffectedResponse); // §12A.7, including --uncommitted
  rpc Watch(WatchRequest) returns (stream WatchEvent);          // path changed, HEAD/ref moved, staging changed, op started/finished, switch done
  rpc Stage(WsStageRequest) returns (WsStageResponse);          // add/remove paths or hunks; the plugin's "git add"
  rpc ReadFile(WsReadFileRequest) returns (stream ReadFileResponse); // §24.4.2: a file at rev:path, a FileId or a stage (0-3) from the local store; offline; never reads the working tree [audit: ide-plugins-folded]
  rpc Ping(PingRequest) returns (PingResponse);                 // version, pid, uptime
  rpc Mount(MountRequest) returns (MountResponse);              // ws_id → starts provider
  rpc Unmount(UnmountRequest) returns (UnmountResponse);
  rpc Switch(SwitchRequest) returns (SwitchResponse);           // ws_id, from_root, to_root, mode {SAFE|MERGE|FORCE}
  rpc Journal(JournalRequest) returns (JournalResponse);        // since token → paths + new token + fresh flag
  rpc ClearJournal(ClearJournalRequest) returns (ClearJournalResponse);
  rpc Dehydrate(DehydrateRequest) returns (DehydrateResponse);  // ws_id, pathspec
  rpc Prefetch(PrefetchRequest) returns (PrefetchResponse);     // ws_id, pathspec
  rpc Stats(StatsRequest) returns (StatsResponse);              // hydrations by path/process, bytes, journal size
  rpc Shutdown(ShutdownRequest) returns (ShutdownResponse);
}

// Plugin messages [audit: plugin-api-undefined]. Ids are 20-byte git SHA-1s (§11.1). workspace.proto imports
// asc/v1/repo.proto (DiffFormat, LogRequest, DiffResponse, LogResponse, BlameResponse) and asc/v1/projects.proto
// (AffectedRequest, AffectedResponse, §12A.8).
message WsStatusRequest { string ws_id = 1; repeated bytes paths = 2; bool untracked = 3; bool ignored = 4; }
message WsFileStatus { bytes path = 1; string xy = 2; /* git porcelain v2 XY */ bytes orig_path = 3; uint32 mode_head = 4;
  uint32 mode_index = 5; uint32 mode_worktree = 6; bytes git_head = 7; bytes git_index = 8; }
message WsStatusResponse { string head_ref = 1; bytes head = 2; string upstream = 3; uint32 ahead = 4; uint32 behind = 5;
  repeated WsFileStatus files = 6; string in_progress = 7; /* merge|rebase|cherry-pick|revert|bisect|empty */ string token = 8; }
message WsDiffRequest { string ws_id = 1; string from = 2; string to = 3; /* "index" | "worktree" | revspec */
  repeated bytes paths = 4; DiffFormat format = 5; uint32 context = 6; }
message WsLogRequest { string ws_id = 1; LogRequest query = 2; }
message WsBlameRequest { string ws_id = 1; string rev = 2; /* empty = working copy */ bytes path = 3; uint32 start_line = 4;
  uint32 end_line = 5; }
message WsAffectedRequest { string ws_id = 1; AffectedRequest query = 2; bool uncommitted = 3; }
message WsAffectedResponse { AffectedResponse result = 1; }
message WatchRequest { string ws_id = 1; string since_token = 2; }
message WatchEvent { enum Kind { KIND_PATHS = 0; KIND_HEAD = 1; KIND_REF = 2; KIND_STAGE = 3; KIND_OP_STARTED = 4;
  KIND_OP_FINISHED = 5; KIND_SWITCH_DONE = 6; KIND_RESYNC = 7; } Kind kind = 1; repeated bytes paths = 2; string ref = 3; bytes old = 4; bytes new = 5; string op = 6;
  string token = 7; }
message WsStageRequest { string ws_id = 1; repeated bytes add = 2; repeated bytes remove = 3; bytes patch = 4; /* git apply --cached format */ }
message WsStageResponse { WsStatusResponse status = 1; }
message WsReadFileRequest { string ws_id = 1; oneof at { WsRevPath rev_path = 2; bytes file_id = 3; WsStagePath stage = 4; }
  uint64 offset = 5; uint64 length = 6; }
message WsRevPath { string rev = 1; bytes path = 2; }
message WsStagePath { uint32 stage = 1; /* 0..3 */ bytes path = 2; }
// ReadFileResponse is repo.proto's; the stream chunks the content. A missing object is hydrated through the tiers; the working tree is never read.
// The internal RPCs (Ping … Shutdown) carry the fields named in their comments; their messages are defined in U41
// and are not part of the stable plugin API.
```

The plugin RPCs are additive like `asc.v1` (§7 Compatibility), and every one also has an `asc … --json` CLI form (§11.2: `status|diff|log|blame --json`, `asc affected --json`, `asc watch --json`; `asc add` for `Stage`), so plugins can use either. ASC's IDE plugins reach all of them through `asc api --stdio` (§24.2), never through the pipe directly.

**HTTP on the same port** (`axum`): `/v8/*` turbo remote cache (§12.4), `/actions/v1/*` Actions cache and artifact manifests (§12.7), `/hooks/forgejo` (mirror webhook), `/.well-known/asc`, `/mcp` (MCP Streamable HTTP, primary role only, §22.2) and `/.well-known/oauth-protected-resource/mcp` (RFC 9728 metadata), `/seeds/<owner>/<repo>/latest` (§12.3), `/healthz`, `/readyz`, `/metrics`.

**Compatibility:** `asc.v1` evolves additively only (new fields, RPCs and enum values). A breaking change is `asc.v2`, served side by side. Envelope versions are independent: readers reject unknown versions with "upgrade asc".

---

## 8. Server (`asc serve`, crate `asc-serve`)

### 8.1 Process and config

One process, one port, stateless apart from caches. Several replicas may run behind a load balancer: every ref change is a SQL CAS, and in-memory graphs refresh on CAS miss. Config `asc-serve.toml`:

```toml
listen = "0.0.0.0:7890"
public_url = "https://asc.forge.example.org"
tls = { cert = "...", key = "..." }          # or terminate at the proxy
store = "s3://asc?endpoint=https://garage.internal:3900&region=garage&path_style=true"   # or file:///srv/asc
db = "postgres://asc_hub@db/forgejo?options=-csearch_path%3Dasc_hub"                              # or sqlite:///srv/asc/asc.db
cache = { memory = "4GiB", disk_dir = "/var/cache/asc-serve", disk = "50GiB" }
[forgejo]
url = "https://forge.example.org"
internal_token_file = "/etc/asc/forgejo-internal-token"
webhook_secret_file = "/etc/asc/webhook-secret"
repo_root = "/var/lib/forgejo/data/forgejo-repositories"   # needed from Stage 2 (bridge)
[turbo]
trusted_tokens_file = "/etc/asc/turbo-trusted"   # Stage 1 only (§12.4)
pr_retention_days = 14
trusted_retention_days = 60
[mcp]                      # §22.8; the /mcp endpoint
enabled = false            # true by default once the auth layer exists (M13); true is refused before then
write = false              # write tools also need the asc:write scope
allowed_origins = []
hmac_keys = []             # key files (32 random bytes each) shared by all replicas
max_tokens = 6000
max_inflight_per_principal = 8
[codeintel]                # §23.10; absent or enabled = false: no CodeIntel service
enabled = false
memory = "2GiB"
workers = 4
[codeintel.embed]
model = "jina-embeddings-v2-base-code"
device = "cpu"
provider = "local"
```

`--role indexer` [audit: codeintel-folded] runs only the codeintel workers (no RPC serving), for GPU hosts (§23.5).

### 8.2 SQL schema (`crates/asc-serve/migrations/{postgres,sqlite}`)

```sql
repos(id BIGINT PK, name TEXT UNIQUE, ns BLOB, mode TEXT /*mirror|primary*/, default_branch TEXT,
      windows_safe BOOL, forgejo_reads TEXT /*git|asc*/, bridge TEXT /*off|lazy|eager*/, land_branches TEXT /*json*/,
      created BIGINT)
refs, ref_log                                  -- §3.4
cs_graph(ns BLOB, cs BLOB, gen BIGINT, parents BLOB /*concat 32B ids*/, PRIMARY KEY(ns, cs))
git_map(ns BLOB, git BLOB, kind SMALLINT /*1 file,2 tree,3 changeset,4 tag*/, asc BLOB, PRIMARY KEY(ns, git, kind))
  INDEX git_map_asc (ns, asc)
file_meta(ns BLOB, file BLOB PK2, size BIGINT, git BLOB)               -- server-computed, see 8.4
op_results(op_id BLOB PRIMARY KEY, repo_id BIGINT, result BLOB, at BIGINT)       -- retained 7 days
outbox(id BIGINT PK autoinc, repo_id BIGINT, op_id BLOB, effects BLOB /*json*/, attempts INT, next_at BIGINT,
       done_at BIGINT NULL, last_error TEXT)
mirror_state(repo_id BIGINT PK, last_import BIGINT, last_error TEXT)
gc_marks(run BIGINT, ns BLOB, key BLOB)          -- O01 only
mainline, path_changes                           -- §5.1
land_map, INDEX ref_log_by_ref                   -- §8.9
changes, change_versions, review_state, open_paths, mergeable   -- §20.4 (stacked changes; change_versions are GC roots, §3.6)
cloud_ws, cloud_heads, user_ref_meta, cloud_usage, gc_purge     -- §21.2.3 (commit cloud; cloud_heads are GC roots, §3.6)
ci_docs, ci_doc_langs, ci_chunks, ci_jobs, ci_manifest   -- §23.2, §23.5 (code intelligence; per namespace, derived and rebuildable; migration 0301_codeintel)
-- repos += branchless BOOL, cloud BOOL, shared_refs TEXT /*json*/, legacy_shared TEXT /*json*/; outbox effect kind cache_evict (§21.3.7)
```

**Schema changes are expand/contract [audit: rolling-upgrade-encodings]:** release N only adds nullable columns and tables, and release N+1 may drop what N stopped using, so replicas N and N-1 run together. `asc serve migrate` runs before the rollout, never at replica start.

### 8.3 `UpdateRefs` pipeline (normative order)

1. **Authorise and classify:** write permission on the repo (§15.1). `hooks=SKIP_PRE` requires the Forgejo internal token. In a branchless repo each ref is classified (§21.5.1): a shared ref continues as below; a non-shared `refs/heads/<n>` is rewritten to the pusher's user ref `refs/users/<owner-id>/heads/<n>` (owner-only, per-user cap, quota charge, no pre-receive, no outbox `post_receive`, no bridge); other tags are rejected with `REF_NOT_SHARED` [audit: branchless-folded].
2. **Idempotency:** if `op_results[op_id]` exists, return the stored result.
3. **Mode check:** a repo in `mirror` mode → `MIRROR_MODE`.
4. **Ingest closure** for every new target: walk new changesets until known in `cs_graph` (changesets already ingested through `Ingest`/`Commit` are skipped). For each in topological order, check that all referenced objects exist (`HasObjects`), validate it (§2.6, §8.4), apply the path policy (§2.8), and upsert `cs_graph` and `git_map` rows (eager derivation). Missing objects → `MISSING_OBJECTS` with the keys.
5. **Policy:** for `FAST_FORWARD`, the new value must descend from the current value (or the ref must be absent). For `LAND`, run §8.5 to produce rewritten changesets, then ingest them as in step 4.
6. **Bridge quarantine** (only when `repos.forgejo_reads = git`, Stage 2): export the new git objects (§14.2) into `<bridge>/objects/incoming-asc-<op_id>/` (a fresh object directory) **before** pre-receive **[fix J1: Stage 2 ordering]**.
7. **Pre-receive** (unless `SKIP_PRE`, or no Forgejo is configured): `POST {forgejo}/api/internal/hook/pre-receive/{owner}/{repo}` with `HookOptions` (`OldCommitIDs`/`NewCommitIDs` as git SHAs while `object_format=sha1`; `RefFullNames`; `UserID`/`UserName`/`DeployKeyID` from authz; `GitPushOptions`; `PushTrigger = "asc"`; in Stage 2 `GitObjectDirectory` = quarantine, `GitAlternativeObjectDirectories` = `<bridge>/objects`, `GitQuarantinePath` = quarantine; from Stage 3 these are empty and Forgejo reads through Seam A). Rejection → delete the quarantine, return `HOOK_REJECTED` with Forgejo's message. For land, the hook sees the **rewritten** changesets **[fix: missing-from-all]**.
8. **One SQL transaction [graft: integration]:** CAS every ref (version check); insert `ref_log` rows (`via` = `asc-push`|`land`|`forgejo-web`|`git-push`|`mirror`); insert `op_results(op_id, response)`; insert `outbox(effects = [bridge_promote?, post_receive])`. A CAS miss rolls back. For `LAND`, retry from step 5 (at most 20 times); otherwise return `REF_CONFLICT`.
9. **Respond** with results and `rewritten` pairs.

**Outbox worker:** rows are processed per repo in id order. Effects are idempotent and run in listed order:

- `bridge_promote` (Stage 2–3, and Stage 4 with `bridge=eager`): move the quarantine's objects into `<bridge>/objects` (or, if the quarantine is gone, export missing objects for the new tips deterministically). Then `git update-ref --stdin` setting each bridge ref to **asc's current value** (not the request's new value, so the bridge converges on the truth). No git hooks run.
- `post_receive`: `POST /api/internal/hook/post-receive/{owner}/{repo}` with the old and new values and `PushTrigger="asc"`. Forgejo's `PushUpdates` is idempotent per (ref, new).

Retries use exponential backoff from 1 s to 5 min, indefinitely. The `asc_outbox_lag_seconds` and `asc_outbox_failures_total` metrics alert after 10 failures. A crash at any point after the transaction commits loses nothing **[fix J1: lost CI triggers]**. A crash before it leaves only orphan objects and a quarantine directory, which a janitor removes after 24 h.

### 8.4 Ingest verification **[graft: integration] [fix J2: client-supplied git fields]**

- `PutObjects` re-hashes every `File`, `Chunk`, `Tree`, `Changeset`, `Tag` and `ChangeMeta` (a `ChangeMeta` is accepted only when its `cs` is in the same push closure or already stored, §20.2). For each new `File` it computes `size` and `git` (blob SHA-1), and records them in `file_meta`. Chunked files: when the `ChunkList` arrives, the server streams its chunks once to compute the same metadata.
- Every `Change::Write`, and every `Entry` of a new tree whose `(kind, id, size, git)` differs from the same-named entry in the corresponding tree of `parents[0]`, is checked against `file_meta`: `size` and `git`. Unchanged entries were verified when their tree was first ingested, so ingest cost is O(changed paths × depth) **[audit: 5-ingest-cost]**. Every `Dir` entry's `git` must equal the canonical render hash of the child tree, or an override `o/<git>` whose body, with git's tree header, hashes to `git` and decodes to the same entries must exist. A mismatch → `INVALID_OBJECT`. Client-built trees are accepted only after this check.
- **Raw git extras [audit: client-raw-git-extras]:** `PutObjects`/`Ingest`/`Commit`/`UpdateRefs` reject with `INVALID_OBJECT` any new changeset carrying `extra["git.commit"]` and any new tag carrying `extra["git.tag"]`. Only server-side importers (mirror import, `ImportGitPush`, `asc serve admin import`) create them, through `canonicalise_commit`/`canonicalise_tag` with the round-trip check of §14.1. Existing changesets already in `cs_graph` are not re-checked. Git history reaches a server by server-side import, never by pushing a locally imported history.

### 8.5 Land (pushrebase) [graft: performance/simplicity]

Applies only when the client explicitly asks for it: `asc push --land`, or the push option `-o land` (`git push -o land` through the shim is the same command) **[audit: push-land-semantics]**. `repos.land_branches` lists the branches that accept LAND requests; it never changes a plain push. A plain push is always the CAS (`FAST_FORWARD`, or forced) and gives git's rejection output (`! [rejected] master -> master (fetch first)`).

1. The stack = new changesets from the pushed tip back to `B`, the first ancestor already in `cs_graph` that is an ancestor of the current tip `T`.
2. If `B == T`, it is a fast-forward.
3. Otherwise compute `A = ∪ paths(stack)` and `P = paths(B..T)` (§4.5). When they intersect, land merges the intersecting files (§4.4) instead of refusing **[audit: 28-agent-conflict-api]**. It returns `LAND_CONFLICT`, with `Conflict` records in the `ErrorDetail`, only when a text merge conflicts or an intersecting path matches `land.noMergeGlobs` (default `bun.lock`, `Cargo.lock`, `uv.lock`, `**/packages.lock.json`). The client then runs `asc pull --rebase`, or an agent uses `Rebase` with resolutions (§8.11). Per-repo `land.contentMerge = false` restores path-disjoint-only landing.
4. If any stack changeset has `git.gpgsig` and the branch requires signed commits (from authz, §15.1) → `LAND_NEEDS_CLIENT_REBASE` (signatures cannot survive a server rebase).
5. Metadata-rebase each changeset onto the new tip in order (§4.4). Committer is unchanged; there is no server signature.
   - 5a. **[audit: land-rewrite-signature-and-raw-extra]** A rewritten changeset drops `extra["git.gpgsig"]` and `extra["git.commit"]` (a raw body names the old parent and cannot be reused). Every other field is kept (author, committer, message, `asc.*` extras). When any signature was dropped, `UpdateRefsResponse.message` = `signatures removed by server rebase: <n> (use 'git pull --rebase' and push again to keep them)`, which the client prints as a `hint:` line. Policy then runs on the rewritten, unsigned changesets (pipeline step 7); a branch that requires signatures would reject them, which is why step 4 returns early.
6. Continue with pipeline step 6. The response's `rewritten` maps old → new ids. The client prints git's push summary for the rewritten tip (`To <url>` and ` + <old>...<new> master -> master (landed: rebased onto <T>)`), moves its local branch and remote-tracking ref to the new tip, and rewrites local branches that pointed into the stack.

No queue and no leader: concurrent lands race on the CAS and retry (step 8). At ASC's rate (peak about 50 per minute) this is ample.

**Legacy git pushes [audit: git-push-land-tracking-refs]:** pushes that arrive through `ImportGitPush` (legacy git clients, Stage 2+) use `UPDATE_MODE_FAST_FORWARD` even on land branches, because git cannot be told about rewrites. A non-fast-forward is rejected with git's `! [rejected] <src> -> <dst> (fetch first)`.

### 8.6 Mirror mode (Stage 1)

- `RepoAdmin.init { mode = "mirror", mirror_url }`. The server keeps a bare mirror at `<cache disk_dir>/mirrors/<repo-id>.git`.
- **Triggers:** a Forgejo push webhook to `/hooks/forgejo` (HMAC-SHA256 `X-Forgejo-Signature` verified with the configured secret), plus a poll every 60 s. Each trigger runs `git fetch --prune <mirror_url> +refs/heads/*:refs/heads/* +refs/tags/*:refs/tags/* +refs/pull/*:refs/pull/*` with a read token, then imports new commits incrementally (§14.1), then sets asc refs to match the git refs with `via = mirror`, all in one transaction. Imports are idempotent, so duplicate webhooks are harmless.

### 8.7 Server-side mutations (`Commit`, `Merge`, `Apply`, `CopyClosure`, `RepoAdmin`)

- `Commit` builds a changeset from `FileOp`s on `parent` (content inline or by `FileId`), then runs the full `UpdateRefs` pipeline on `update_ref` with `expect`.
- **A server `Commit` writes a `ChangeMeta` [audit: mcp-folded].** The changeset it creates is a draft change (`op = New`, `preds = []`), written in pipeline step 4 through the `IngestObserver` hook (§20.4; unit M18). Changes created through MCP `commit` or Forgejo's web editor therefore get a change id. MCP `commit` accepts only an empty `update_ref` or `refs/for/*` (§22.3.3).
- **Changesets without refs [audit: 9-ingest-without-ref]:** creating a changeset never requires a ref move (Scion).
  - `Commit` with an empty `update_ref` runs only pipeline step 4: closure check, §2.6 and §8.4 validation, path policy, eager derivation into `cs_graph`/`git_map`. It returns the id. No ref, hook, `ref_log` or outbox row is involved.
  - `Ingest` does the same for changesets already uploaded with `PutObjects`. It requires write permission.
  - An ingested changeset can be used by id at once in every read RPC (`Diff`, `Log`, `MergeBase`, `ReadTree`, `Affected`, `Classify`).
  - Unreferenced ingested changesets stay GC roots for 14 days (§3.6).
  - CLI: `asc push --no-ref <rev>` (asc-only flag) uploads the closure, calls `Ingest` and prints the changeset id.
- `Merge` with `dry_run` is Forgejo's mergeability check: it returns conflicts and the changed paths that match `protected_globs`. Styles: `MERGE` (two-parent changeset), `SQUASH` (one changeset), `REBASE` (metadata rebase of each commit), `REBASE_MERGE` (rebase then a merge commit), `FAST_FORWARD_ONLY`. With `update_ref`, the result goes through `UpdateRefs` so Forgejo hooks run once. When `head_repo` differs, `CopyClosure` runs first.
- `RepoAdmin`: `init`, `fork` (new namespace plus closure copy job), `delete` (refs dropped; the namespace is deleted after 30 days), `rename` (registry only), `size` (logical = bytes reachable from refs; unique = namespace bytes), `verify` (closure of every ref exists; git map round-trip).

### 8.8 Expiring refs

`UpdateRefs.ttl_seconds` (from `asc push -o asc.ttl=7d`) sets `refs.expires_at`, and is accepted only for `refs/heads/agent/*` and `refs/snapshots/*`. A worker deletes expired refs every 10 minutes through the normal pipeline (outbox `post_receive` with a deletion), so Forgejo's branch table stays in sync **[graft: performance]**. In a branchless repo the same TTL applies to the pusher's `agent/*` user refs, and a user ref whose stack has landed is deleted `landed_ref_days` (default 7) after `user_ref_meta.landed_at`, which the land transaction sets (§21.2.4) [audit: branchless-folded].

### 8.9 Landing map and FindLanding [audit: 23-landing-index]

```sql
land_map(ns BLOB, old_cs BLOB, new_cs BLOB, ref TEXT, ref_log_seq BIGINT, op_id BLOB, PRIMARY KEY(ns, old_cs))
  INDEX land_map_new (ns, new_cs)
INDEX ref_log_by_ref (repo_id, name, seq)
```

- The `UpdateRefs` transaction (§8.3 step 8) inserts one `land_map` row for every `rewritten` pair: land, and `Merge` with `REBASE`, `REBASE_MERGE` and `SQUASH` (for `SQUASH`, every source changeset maps to the result). Rows are kept forever.
- `FindLanding(changeset, ref)`: map through `land_map` (old → new), else use the id itself. Look up its position in `mainline` (§5.1). The answer is the first `ref_log` entry for that ref whose new tip has a position ≥ that position, found by binary search over `ref_log_by_ref`. Positions only grow on fast-forward and land refs, so this is O(log n).
- **Rule:** no asc component, Forgejo patch, CI tool or documented agent workflow finds a landed id or a merge time by walking `Log` or matching messages. They call `FindLanding`, or use Forgejo's PR `merged_commit_id` for PR-level questions.
- CLI: `asc landed <rev> [--ref <b>] [--json]` (asc-only command).

### 8.10 Change classification (`Classify`) [audit: 27-semantic-classification]

- Derived type `classify.v1` (lazy; per changeset against its first parent). `Classify(base, head, merge_base)` returns a class per file and `overall` = the maximum.
- `ChangeClass`, in order: `RENAME_ONLY` (same FileId), `MODE_ONLY`, `WHITESPACE_ONLY`, `COMMENT_ONLY`, `DOCS`, `GENERATED`, `LOCKFILE`, `TEST`, `LOGIC`.
  - `WHITESPACE_ONLY`: equal after removing whitespace. For indentation-significant files (`.py`, `.yaml`, `.yml`, `Makefile`) only trailing whitespace is ignored.
  - `COMMENT_ONLY`: the token streams are equal after a per-extension lexer drops comments. The lexer handles `//` and `/* */` (rs, ts, tsx, js, jsx, cs, go, c, cpp, java), `#` (py, toml, yaml, sh) and `<!-- -->` (md, html), and skips string literals (Rust raw strings, Python triple quotes, C# verbatim strings). Doc comments count as comments.
  - `DOCS`, `TEST` and `LOCKFILE` come from `.asc/review.toml` globs. Defaults: `**/*.md` and `docs/**`; `**/*test*/**`; `bun.lock`, `Cargo.lock`, `uv.lock` and `**/packages.lock.json`. `GENERATED` comes from the `linguist-generated` attribute.
- **Soundness rule:** a false `LOGIC` is acceptable; a false lower class is a bug. Any lexer error, unknown extension or binary file is `LOGIC`.
- **Review integration (K17):** Forgejo branch protection gains `asc_relaxed_classes` (default empty) and `asc_relaxed_approvals`. A PR whose files are all in the relaxed classes needs `asc_relaxed_approvals` approvals instead of the normal count. Forgejo shows a per-file class badge. Protected-file globs always override relaxation.
- **Clients:** `asc diff --classify` (asc-only flag); `asc ci changed` writes `semantic=<overall>`.

### 8.11 Server rebase and conflict resolution for agents [audit: 28-agent-conflict-api]

- `Rebase(base, head, onto, resolutions, dry_run, update_ref, expect, mode)` replays `base..head` onto `onto` on the server.
- A conflict is returned as a `Conflict { changeset, path, kind, base_id, ours_id, theirs_id, marked_id }`. `marked_id` is the FileId of the 3-way result with git markers (`diff3` style), written to the namespace and readable with `ReadFile`.
- An agent resubmits the same request with `Resolution { changeset, path, op }` entries. The server applies them while replaying the stack, and succeeds when no conflict is left unresolved. With `update_ref`, the result goes through `UpdateRefs` (mode `LAND` or `FAST_FORWARD`), so hooks run once. The request is idempotent by `x-asc-op-id`.
- `MergeResponse.conflicts` and `ApplyResponse.conflicts` gain `conflict_details` (additive). Land content merge is §8.5 step 3.

---

## 9. Machine side: daemon, workspaces, synthetic `.git`

### 9.1 The daemon (`asc daemon`)

One per user per machine. It exists because a virtual filesystem needs a long-lived process. It also hosts background work.

| Does | Does not |
|---|---|
| Host every virtual workspace (ProjFS root or FUSE mount) | Proxy history RPCs (the CLI calls the remote directly) |
| Keep each workspace's journal; reconcile on start | Own the cache (a shared directory) |
| Run switch for virtual workspaces (placeholder updates) | Make a command wait for an upload (uploads are asynchronous, §21.3.1) |
| Upload new drafts and sync cloud heads in the background, in branchless repos (§21.3) | |
| Prefetch, GC the cache, dehydrate idle files, expire ephemeral worktrees | Run real git |
| Serve the plugin API (`Status`, `Diff`, `Log`, `Blame`, `Affected`, `Watch`, `Stage`, `ReadFile`; §7) for ASC's IDE plugins, which reach it through the `asc api --stdio` bridge (§24.2) | |
| Serve the `CodeIntel` IPC from a lazily built overlay of dirty files; close local index readers after 5 min idle (§23.2.5) | Load an embedding model or build an index (the `asc codeintel worker` and `asc-embed` helpers do, in separate processes) |

- **Mapping to Scion's per-node daemon [audit: 14-node-daemon-mapping]:**
  - On developer machines the per-user daemon plus the shared `$ASC_HOME/cache` play scion-daemon's role: every workspace and worktree of the user reads one cache.
  - On CI hosts no daemon runs inside jobs. The host cache (§12.3) is the node's object store. Only the trusted warmer fills it, and jobs read it read-only. This prevents cross-job poisoning and denial of service without a node service that holds job credentials.
  - Accepted cost: objects that are not on trunk (PR changes) are fetched once per job, not once per host. An ASC PR touches a median of 4 files, so this is kilobytes. A host-level fetch proxy is not built.
- **Start:** the CLI starts it on first need. The installer registers a per-user logon task (Windows Task Scheduler: restart on failure 3 times, 10 s apart) or a systemd user unit (`Restart=on-failure`, `RestartSec=2`) **[fix: missing-from-all, watchdog]**. Stale pid files are detected by `Ping`.
- **IPC:** `\\.\pipe\asc-<user-SID>` with a DACL that allows only the user's SID and SYSTEM; Unix `$XDG_RUNTIME_DIR/asc/asc.sock` (or `$ASC_HOME/run/asc.sock`), mode 0600, in a 0700 directory.
- **Version skew:** the CLI compares `Ping.version` with its own. On mismatch it asks the daemon to `Shutdown` (which stops providers cleanly) and starts the new binary. ProjFS roots re-attach with the same instance GUID; FUSE remounts at the same path. The CLI stops a daemon of another version **before** it runs `state.db` migrations (§9.2).
- **Liveness [audit: daemon-hang-watchdog]:** the CLI calls `Ping` with a 2 s timeout. On timeout it writes a dump (`asc debug trace` of all threads to `log/`), kills the daemon process and starts a new one. The daemon's own watchdog thread aborts the process when a ProjFS/FUSE callback has run for longer than 60 s, and the service manager restarts it. Fallback: `asc worktree convert --plain <path>` materialises a virtual workspace in place, without the daemon, when virtual mode cannot be used.
- **Daemon down while files are open:** hydrated and full files stay readable and writable (they are real NTFS/overlay files). Reads of virtual files fail until restart (≤ 10 s with the watchdog). Writes during downtime are found by reconcile (§10.2.7). `asc status` prints `daemon restarted at <t>; reconciled N paths`.
- **No daemon** (`ASC_NO_DAEMON=1`, CI, tests): the CLI does everything in-process. Only plain workspaces are available.

### 9.2 State database (`$ASC_HOME/state.db`, SQLite via sqlx)

```sql
meta(schema_version INT, machine_id BLOB /*16 random bytes, §3.4*/)   -- one row
repos(id TEXT PK /*uuid*/, name TEXT, ns BLOB, remote_url TEXT, remote_kind TEXT, default_branch TEXT,
      windows_safe BOOL)                              -- display ids are the per-user config asc.displayIds (§11.1)
refs, ref_log                                         -- SqlRefs schema (§3.4), repo_id = rowid of repos
cs_gen(repo_id, cs BLOB, gen INT, parents BLOB, PRIMARY KEY(repo_id, cs))
history_boundary(repo_id, cs BLOB, PRIMARY KEY(repo_id, cs))   -- oldest local changesets whose parents are not local (§6.3)
git_map(repo_id, git BLOB, kind INT, asc BLOB, PRIMARY KEY(repo_id, git, kind)) + index (repo_id, asc)
workspaces(id TEXT PK, repo_id, path TEXT UNIQUE /*requested path*/, mount TEXT /*mount root; differs for nested paths, §9.3*/,
           mode TEXT /*virtual|plain*/, head TEXT /*ref name or ""*/,
           base_cs BLOB, epoch INT, epoch_time INT /*unix s*/, sparse TEXT, ephemeral_until INT NULL,
           locked TEXT NULL, created INT)
stage(ws_id, set_id INT NOT NULL DEFAULT 0 /*0 = the workspace's staging*/, path BLOB, stage INT /*0 normal, 1-3 conflict*/,
      kind INT, file BLOB NULL /*NULL = staged delete*/, size INT, git BLOB, intent_to_add BOOL,
      PRIMARY KEY(ws_id, set_id, path, stage))        -- staging (§9.5)
stage_sets(ws_id, set_id INT, index_path TEXT UNIQUE, created INT, PRIMARY KEY(ws_id, set_id))   -- temporary sets (§9.5)
entry_times(ws_id, path BLOB, time INT, PRIMARY KEY(ws_id, path))   -- placeholder time overrides (§10.1)
journal(ws_id, seq INT, path BLOB, op INT, PRIMARY KEY(ws_id, seq))  -- op: 1 modified,2 created,3 deleted,4 renamed-from,5 dir-renamed-from,6 dir-renamed-to
switch_intent(ws_id PK, from_root BLOB, to_root BLOB, to_head TEXT, pending BLOB /*paths*/, started INT)
ws_ops(ws_id PK, pending_renames BLOB)                -- only pending `asc mv` renames; in-progress ops are git state files (§4.4)
inodes(ws_id, ino INT, path BLOB, PRIMARY KEY(ws_id, ino)) + UNIQUE(ws_id, path)   -- FUSE only
changes, change_preds, op_log, stack_frozen, review_cache   -- stacked changes and the operation log (§20.3)
draft_heads, cloud_queue, cloud_state, cloud_base           -- branchless drafts and the commit-cloud queue (§21.2.2)
```

The journal is appended in batches (≤ 100 ms) from an in-memory queue. A crash can lose hints but never correctness, because reconcile recovers them (§10.2.7).

- **One migrator [audit: sqlx-two-migrators]:** `state.db` has exactly one migrator (`asc-state`). Its `0001_state.sql` contains the `refs` and `ref_log` DDL of §3.4 verbatim. `SqlRefs::new` never migrates; `SqlRefs::migrate()` is called only by `asc serve` on the server database. A test asserts that `PRAGMA table_info` of `refs`/`ref_log` is equal under both migration sets.
- **Schema version:** an older binary that meets a newer `schema_version` exits with `fatal: asc: state.db is from asc <v>; run asc upgrade`.

### 9.3 Workspaces and worktrees

A workspace is a `workspaces` row, a synthetic gitdir `wc/<id>/git/`, a `.git` gitfile at its root, and a mount (virtual) or materialised files (plain).

- `asc clone <url> [<dir>] [--mode virtual|plain] [--branch b] [--sparse <cone>…] [--no-checkout] [--reference <git-dir>] [--seed <url>]`: register the repo, fetch (§6.3), create the workspace, write the synthetic `.git` (§9.4), mount or materialise. `asc init`/`asc clone` add `.claude/worktrees/` to the repo-local exclude file in effect (§2.4.2).
- `asc worktree add <path> [-b <new>] [--detach] [<rev>] [--ephemeral --ttl 7d] [--mode …]`: the same with no network when the revision is cached. Cost: rows, plus one index file (about 0.5 MB for ASC) when `asc.turboIndex` is on (§9.4).
- `asc worktree list [--porcelain]|remove [--force]|prune|lock|unlock|move` mirror git, including git's output formats. `remove` unmounts, deletes the root (including redirections) and the gitdir, and refuses when dirty unless `--force`. In plain mode `worktree` needs neither the daemon nor any VFS code, so cheap worktrees ship in phase 1.
- **Branch in use [audit: worktree-layout-and-git-worktree]:** asc applies git's worktree rules across all workspaces of a repo on the machine. `switch`/`checkout` of a branch that another workspace has checked out fails with git's `fatal: '<b>' is already used by worktree at '<path>'`; `branch -d|-D|-m` of such a branch fails with git's error.
- **Nested paths:** when the requested path lies inside another workspace's root (Claude Code's default is `<repo>/.claude/worktrees/<name>`), asc mounts the new workspace at `$ASC_HOME/wt/<ws-id>` and creates a directory symlink (Developer Mode) or junction at the requested path; `workspaces.path` is the requested path and `workspaces.mount` the mount root. The parent workspace's status treats the path as git does: a directory that contains `.git` is reported as one untracked entry (`?? <dir>/`) unless ignored, and is never descended. Spike S11 confirms that ProjFS serves a junction inside a virtualization root that points to another root. If S11 fails, nested paths use plain mode.
- **Ephemeral worktrees [graft: integration/performance]:** `--ephemeral --ttl <dur>` sets `ephemeral_until`. The daemon removes expired clean worktrees hourly. Expired dirty worktrees are kept, and `asc status` warns. Agent branches use `refs/heads/agent/<name>` and push with `-o asc.ttl=<dur>`.
- **Claude Code agents [audit: claude-code-worktrees]:**
  - `asc internal claude-hook worktree-create` reads the hook JSON from stdin (it uses the worktree name field and `cwd`) and runs the equivalent of `asc worktree add --ephemeral --ttl 7d -b agent/<name> <path> <base>`, where `<path>` = `<repo-root>/.claude/worktrees/<name>` and `<base>` = the current `HEAD`. The nested-path rule applies. It prints the absolute path on stdout and nothing else; errors go to stderr with a non-zero exit.
  - `asc internal claude-hook worktree-remove` reads the worktree path from the JSON and runs `asc worktree remove --force`. Unpushed commits stay reachable from `refs/heads/agent/<name>`.
  - ASC's `.claude/settings.json` configures both hooks with these commands.
  - With no hook configured, Claude Code's `git worktree add`/`git worktree remove` reach asc through the shim (§11.4) and are native.
  - Spike S7 confirms the JSON field names and the stdout contract against the pinned Claude Code version. The compat suite records Claude Code's EnterWorktree/ExitWorktree traces with and without hooks (§17.1).

### 9.4 The synthetic `.git` (Turborepo only) [audit: user decision No-IDE]

```
<root>/.git                              gitfile: "gitdir: $ASC_HOME/wc/<ws>/git\n"
$ASC_HOME/wc/<ws>/git/
  commondir                              "../../../repos/<repo-id>/.git\n"
  HEAD                                   "ref: refs/heads/<b>\n", or the git SHA when detached
  index                                  rendered by asc from tree metadata plus stage set 0 when asc.turboIndex is on (below); nothing else writes it
  asc-workspace                           the workspace id: the shim's marker (§11.4)
  asc-commit-index                        temporary stage set rendered for hooks of `commit <paths>`/`--only`/`-i` (§9.7)
  MERGE_HEAD, rebase-merge/, COMMIT_EDITMSG, …   in-progress state in git's format (§4.4)
$ASC_HOME/repos/<repo-id>/.git/           common dir, shared by the repo's workspaces on the machine
  config                                 git-format config (below)
  info/exclude, hooks/                   as in git
```

- **Purpose:** turbo reads `.git/index` with gix-index and asks `git` a few questions. The gitdir holds nothing else: no objects, no refs, no packed-refs, no reflogs, no rendered commits. Real git does not operate on it (§9.6). This replaces v1.0's full projection, git-index staging, refs mirror and `git-remote-asc` helper.
- **Config:** `core.bare=false`, `core.autocrlf=false`, `core.ignorecase=<true on Windows>`, `core.symlinks=<os>`, `core.filemode=<os>`, `core.longpaths=true`; `remote.<n>.url` = the user's remote URL exactly as given to `clone` or `remote add`, and `remote.<n>.fetch` (including `^refs/heads/agent/*`, §2.7); `branch.<b>.remote`/`.merge`; and any setting the user or a tool writes with `git config --local` (for example husky's `core.hooksPath`). asc reads and writes this file with `gix-config`. `git remote get-url origin` and `git remote -v` therefore print the real URL, which Claude Code and `gh` use to find the forge repo **[audit: remote-url-projection]**.
- **Conditional includes [audit: includeif-identity]:** asc's config loader evaluates `includeIf "gitdir:"`/`"gitdir/i:"` against `<workspace root>/.git`, the path a plain clone at that location would have, so a `[includeIf "gitdir:~/work/"]` identity applies as it would for git. `asc doctor` re-checks this, reports drift, and recommends `hasconfig:remote.*.url:` conditions, which do not depend on the gitdir location.
- **When it exists [audit: synthetic-index-o-files]:** the index is written only when `asc.turboIndex` is true. The default `auto` is true when the root tree has a `turbo.json` entry or the workspace is plain (plain status uses the index as its stat cache, and plain materialisation is O(files) anyway). A virtual workspace without it has no `index` file; its status uses the journal (§10.2).
- **Index (a write-only export, not the staging area) [audit: turbo-index-reflects-head-breaks-staged-contract]:** v2 with a TREE (cache-tree) extension, rendered from HEAD's tree metadata overlaid with stage set 0 (§9.5): staged writes and intent-to-add entries replace or add entries (oid and size from the `stage` row), staged deletes remove them, and conflict stages 1–3 are written as git writes them. Each entry: path; mode; `git` oid; size; ctime = mtime = the placeholder time (virtual, §10.1) or the real stat (plain); dev/ino 0 on Windows, `inodes.ino` on FUSE, real on plain Unix; uid/gid 0 on virtual. No skip-worktree bits except unrepresentable entries (§2.8) and plain sparse mode. Gitlinks: mode 160000.
  - It is written atomically with git's lock protocol: create `index.lock` with `O_EXCL`, write, fsync, rename. If the lock is held, asc waits up to 5 s with backoff and then fails with git's message (`Unable to create '…/index.lock': File exists`) **[fix: missing-from-all, concurrent index writers]**.
  - After writing, asc sets the index file's mtime to `max(now, max_entry_time + 2 s)` so no entry is racy.
  - asc updates it on clone, switch, checkout, commit, reset, restore, merge, rebase, add, rm, mv, stash, apply --cached, update-index and read-tree by patching the previous index with the changed paths only (no reads of unchanged trees), then rewriting the file. It mirrors set 0, as git's index does; asc still never reads it as staging (§9.5). Turbo stat-checks every entry and hashes any file that differs, so its hashes stay correct.
  - asc also reads it back as its own stat cache for plain-mode status. Nothing else writes it.
- **Why the common dir sits at `repos/<id>/.git`:** turbo treats a gitfile worktree as linked and puts its cache under the main worktree root, which is the parent of the common dir. All workspaces of a repo therefore share `$ASC_HOME/repos/<id>/.turbo/cache` (a machine-wide turbo cache). Contract-tested in U61p.
- **Turbo's git calls** (`rev-parse`, `ls-files --others`, `diff-tree --merge-base`, `show <base>:bun.lock`, `status`) reach asc through the shim (§11.4) and are native. The shim MUST precede real git on the PATH that turbo sees; `asc doctor` checks it. Spike S4 records every git invocation turbo 2.10 makes, so the native table (§11.5) covers them, and confirms that turbo's index reading works with this layout.

#### 9.4.1 Hooks run by native commands [audit: hooks-full-set]

asc runs git hooks itself; there are no dispatcher scripts. The hooks directory is `core.hooksPath`, else `<common>/hooks`. A relative `core.hooksPath` resolves against the workspace root, so husky's `.husky/_` works. On Windows hooks run through `<GitForWindows>\usr\bin\sh.exe` (found from `RealGit`, §11.4), never the `bin\sh.exe`/`bin\bash.exe` wrappers, which put real git first on `PATH`.

**Child PATH (MUST) [audit: hook-child-path-real-git, hook-child-path-shim]:** every child that asc spawns (hooks, `!` aliases, `core.editor`/`GIT_EDITOR`/`GIT_SEQUENCE_EDITOR`, `diff`/`merge` drivers and textconv, `rebase --exec`, `bisect run`, `git-<name>`/`asc-<name>` external commands) gets `PATH = <private shim dir> + PATH_SEPARATOR + PATH`, as git puts its exec-path first for its children. The private shim dir is `<exe dir>/shim` (§11.4): the installer and `asc upgrade` create it, it always exists whether or not the user installed the shim for interactive use, and asc recreates it on first use if it is missing (`fatal: asc: cannot create <dir>; run asc doctor --fix` if that fails). It is never added to the user's own PATH by this rule. On Windows a hook's PATH is `<private shim dir>;<GitForWindows>\mingw64\bin;<GitForWindows>\usr\bin;<inherited PATH>`. `git` in a child, including husky scripts and lint-staged's `execa('git')`, therefore always reaches asc. `RealGit` children (§9.6) are the only exception and keep the inherited PATH.

Native commands MUST run the same hooks at the same points as git 2.55, with git's argv, stdin, cwd (the worktree root) and env:

- `commit`: `pre-commit`, `prepare-commit-msg <file> [message|template|merge|squash|commit [<sha>]]`, `commit-msg <gitdir>/COMMIT_EDITMSG`, `post-commit`, and `post-rewrite amend` (stdin `<old> <new>`).
- `merge`/`pull`: `pre-merge-commit` and `post-merge <squash-flag>`.
- `rebase`: `pre-rebase <upstream> [<branch>]`, then `post-rewrite rebase`.
- `checkout`/`switch`/`clone`/`worktree add`: `post-checkout <old> <new> <1|0>`. Clone and worktree add use the null SHA as old.
- `push`: `pre-push <remote> <url>`, with stdin lines `<local ref> <local sha1> <remote ref> <remote sha1>`. It runs before any upload, and a non-zero exit aborts with git's message.
- `am`: `applypatch-msg`, `pre-applypatch` and `post-applypatch`.
- every local ref update (commit, branch, tag, reset, update-ref, rebase, merge): `reference-transaction prepared|committed|aborted` with stdin lines `<old> <new> <ref>` (git SHA-1s) **[audit: hooks-must-vs-list]**.
- **One chokepoint [audit: local-ref-update-chokepoint]:** asc moves local refs through one function, which writes the ref CAS, the reflog, the `reference-transaction` hook calls and, when HEAD's commit changes, the synthetic-index update (§9.4). No command moves a local ref any other way. Fetch and push remote-tracking updates go through it too, as in git.
  - **Engine code never moves a local ref [audit: engine-local-ref-moves-bypass-chokepoint].** An engine function whose effect includes local ref moves (fetch, clone, push's remote-tracking update, restack execution) returns the planned `RefTxn`, and the CLI applies it through the chokepoint.
  - CLI code receives the engine over `ReadOnlyRefs<SqlRefs>`, whose `update` returns `Error::ReadOnly`, so a bypass fails in tests instead of silently skipping hooks, the reflog `op_id` and the op log (§20.3).
- `gc`: `pre-auto-gc` is never run (asc has no auto-gc).

Never run, because asc has no such mechanism: `post-index-change`, `fsmonitor-watchman`, `push-to-checkout`, `sendemail-validate`, `p4-*`, and the receive-pack hooks (`pre-receive`, `update`, `proc-receive`, `post-receive`, `post-update`; server policy is Forgejo's, §8.3). They are listed in `tests/compat/allowed.toml`.

Env as git sets it: `GIT_INDEX_FILE` (absolute) is always set for commit hooks: `<gitdir>/index` for a normal commit, which asc maps to the real staging (set 0, §9.5), or the rendered temporary stage set `<gitdir>/asc-commit-index` for `commit <paths>`/`--only`/`-i` (§9.7) **[audit: commit-precommit-staging-order, git-index-file-stage-sets]**. git passes `<gitdir>/index.lock` for a normal commit; this path difference is listed in `allowed.toml`. Also `GIT_EDITOR=:` when no editor is used, `GIT_PREFIX`, `GIT_REFLOG_ACTION`, and `GIT_DIR` only where git sets it. All ids passed to hooks are git SHA-1s. `--no-verify` skips exactly the hooks git skips (`pre-commit`, `commit-msg`, `pre-push`, `pre-merge-commit`). Git commands that hooks run (`git diff --cached`, `git add`, `git stash create`, …) reach asc through the child PATH above.

### 9.5 Staging area (asc's own) [audit: user decision No-IDE]

- Staging lives in `state.db.stage` (§9.2): one row per path whose staged state differs from HEAD, plus conflict stages 1–3 and intent-to-add entries. A path without a row is staged as in HEAD. git's index file is not the staging area (§9.4).
- `asc add` ingests the content into `store/` (FileId, chunks; normalisation per §2.4.1), computes the git blob SHA-1, and writes the `stage` row plus a `git_map` row. `add -p` stages a hunk-filtered blob the same way. Nothing is read from the working file again at commit time **[fix J1/J2/J3: `git add -p`]**.
- **HEAD moves keep the staging [audit: staging-overlay-breaks-on-head-moves].** Set 0 is an overlay on HEAD, so a HEAD move must keep the staging that git's index would have (git keeps its index on `reset --soft`, on `update-ref` of the checked-out branch and on `symbolic-ref HEAD`).
  - When HEAD's commit changes from `a` to `b`, the chokepoint (§9.4.1) by default rewrites set 0 so that `b ⊕ rows' == a ⊕ rows`: for each path in `leafdiff(a, b)` without a row it adds a row with `a`'s entry (or a staged delete), and it drops rows that now equal `b`. Cost O(leafdiff).
  - Commands that define a new index state (`commit`, `checkout`/`switch`, `reset --mixed|--hard`, `merge`, `rebase`, `cherry-pick`, `revert`, `stash`, `pull`, `bisect`, stack verbs) instead pass the rows they want.
  - The synthetic index (§9.4) is therefore unchanged in content for `reset --soft`, `update-ref` and `symbolic-ref`: after `git reset --soft HEAD~1` the commit's changes are staged, and the next `asc commit` is not empty.
- `ls-files -s`, `diff --cached`, `status`, `write-tree` and the other index plumbing read the HEAD tree overlaid with `stage` rows and print git SHA-1s, exactly as git would print its index.
- **`GIT_INDEX_FILE` → stage set (normative) [audit: git-index-file-stage-sets]:** if `GIT_INDEX_FILE` is unset, or equals (after normalisation) the workspace's own `<gitdir>/index`, asc uses the default stage set (set 0, the `stage` rows). asc never parses that file as staging. Any other path names a temporary stage set, keyed by its absolute path in `stage_sets`:
  - a registered path uses its set; a missing file starts an empty set (git semantics); an existing unregistered file is imported once (git oids → FileIds through `git_map`; an unknown oid fails with git's `fatal: unable to read <sha>`);
  - after any asc command changes a temporary set, asc re-renders the file at that path under git's lock protocol, so tools that pass `GIT_INDEX_FILE` between git calls keep working;
  - a temporary set is dropped when its file disappears, or after 24 h;
  - `commit <paths>`/`--only`/`-i` create a temporary set and export its path to hooks. A normal commit exports `<gitdir>/index`, which maps to set 0 (§9.4.1, §9.7).
- ASC's IDE plugins stage through `Workspace.Stage` (§7) or `asc add`.

### 9.6 Real git and asc workspaces [audit: user decision No-IDE]

- In an asc workspace the `git` command is asc (§11.4), and every invocation runs natively (§11.5). Real git never operates on an asc workspace.
- Real git run directly (without the shim) finds a gitdir with no objects or refs and fails with `fatal: not a git repository`. `asc doctor` reports every PATH surface where the shim is missing (§11.4).
- Commits made by real git are therefore never imported, and there is no reconcile of git-side refs, no refs mirror and no `git-remote-asc` helper. v1.0's projection reconcile and delegation sections are withdrawn.
- asc itself still runs real git, always through `RealGit` (§11.4), for: git-remote mode (§6.3), the Forgejo bridge and export (§8.3, §14.2), `git credential`, and the shim's fall-through outside asc workspaces.

### 9.7 `asc commit` (staging → changeset) [audit: commit-precommit-staging-order]

git runs `pre-commit` first and then re-reads the index, so a hook's `git add` (lint-staged) is committed. asc follows the same order:

0. Snapshot set 0 (copy its rows into a temporary set). If the commit aborts before step 6 (a failing `pre-commit`, `prepare-commit-msg` or `commit-msg`, an aborted editor, an empty message), restore set 0 from the snapshot, as git rolls back `index.lock`; drop the snapshot in every case **[audit: commit-hook-failure-rollback]**.
1. With `-a`, stage every stat-dirty tracked file (journal ∪ stat check) as `asc add -u` would. With `<paths>`/`--only`/`-i`, build the temporary stage set (§9.5) and render it to `<gitdir>/asc-commit-index`.
2. Unless `--no-verify`, run `pre-commit` with `GIT_INDEX_FILE` set to the stage set in use (§9.4.1): `<gitdir>/index` (set 0) for a normal commit, else `<gitdir>/asc-commit-index`.
3. Re-read that stage set (set 0 rows, or the temporary set, which the shim's `git add` updated). Stage changes made by the hook are committed.
4. Run `prepare-commit-msg` and `commit-msg`.
5. Build the root with `apply` and the changes with `leafdiff` (§4.2) from the stage set as re-read: the HEAD tree overlaid with its rows. Every staged row already carries its `FileId`, ingested by `asc add`, so commit reads no file content. Record `copy_from` from `asc mv` (pending renames in `ws_ops`).
6. Sign if `-S`/`commit.gpgSign`, write the changeset to `store/`, derive the eager types, and move the branch (CAS on the old value). Clear the `stage` rows (for `--only`/`<paths>`, set 0 keeps its rows for other paths and takes the committed paths' new state, as git does), drop the temporary set, write `COMMIT_EDITMSG` and the reflog, update the synthetic index (§9.4), then run `post-commit` (and `post-rewrite amend`).

---

## 10. Working copies and virtual filesystems (crates `asc-wc`, `asc-vfs`)

### 10.1 Stat times (all modes) [fix J1/J2/J3: committer-time mtimes]

- Each virtual workspace has an `epoch` counter and an `epoch_time`, both in `workspaces`.
- **Clone/worktree add:** `epoch = 1`, `epoch_time = floor_seconds(now)`. Every placeholder reports `CreationTime = LastWriteTime = ChangeTime = LastAccessTime = epoch_time`.
- **Switch/reset/restore** that changes a set of paths: `T = max(floor_seconds(now), previous epoch_time + 1)`. Then `epoch += 1`, `epoch_time = T`, and every **changed** placeholder is rewritten with time `T` and recorded in `entry_times(ws, path, T)`. Unchanged placeholders keep their times. Times therefore only move forward. A file changed by a switch is always newer than build outputs created before the switch, so mtime-based tools (cargo, `tsc -b`, make) rebuild.
- The index records exactly these times. A path's time = `entry_times[path]` if present, else the workspace's clone-time `epoch_time` (kept in `workspaces.created`). There is no history walk and no per-path derived data.
- Hydration never changes placeholder timestamps. ProjFS keeps `PRJ_FILE_BASIC_INFO`, which spike S1 confirms. FUSE `getattr` reports the same values. A file the user writes gets real times, so it is stat-dirty and is hashed, which is correct.
- `entry_times` is compacted on a clean switch to a new tree when it exceeds 50k rows: rewrite all placeholders with the new `T` and clear the table.

### 10.2 Windows: ProjFS (`asc-vfs::projfs`)

1. **Binding:** our own thin layer over `windows` 0.62.2 (`Win32_Storage_ProjectedFileSystem`). `ProjectedFSLib.dll` is loaded at runtime with `libloading`, so `asc.exe` runs where the feature is off and falls back to plain mode with a notice. The MSI enables `Client-ProjFS` (it runs elevated). `asc doctor --fix` prints the admin command otherwise.
2. **One virtualization root per workspace**, with a stable instance GUID (the workspace uuid). The root directory is marked with `FILE_ATTRIBUTE_NOT_CONTENT_INDEXED`.
3. **Placeholders:** `VersionInfo.ContentID` = 33 bytes `kind ‖ id` (kind 1 = file `FileId`, 3 = tree `TreeId`; kind 2 is unused). `ProviderID` = the 16-byte workspace uuid. Sizes come from tree metadata. Times follow §10.1. Symlinks use `PrjFillDirEntryBuffer2` with symlink extended info when Developer Mode is on. Otherwise they are projected as text files containing the target, as git does with `core.symlinks=false`, and the projection config says so.
4. **Callbacks:**
   - `StartDirectoryEnumeration`/`GetDirectoryEnumeration`/`EndDirectoryEnumeration` answer from the tree at the base, honouring the search expression with `PrjFileNameMatch` and sorting with `PrjFileNameCompare`. Gitlink directories are composed (§10.6).
   - `GetPlaceholderInfo` looks the path up case-insensitively.
   - `GetFileData` serves **the ContentID recorded in the placeholder, never the current HEAD**. It reads `cache/f/<id>` (or fetches through `Tiered`) and writes with `PrjWriteFileData` in 1 MiB aligned buffers (`PrjAllocateAlignedBuffer`). Chunked files stream chunk by chunk. A fetch that is not local returns `ERROR_IO_PENDING` and completes with `PrjCompleteCommand`. Cancellation is honoured.
   - `QueryFileName` and `Notification` are also implemented.
   - No ProjFS call is ever made from inside a callback while holding a lock.
5. **Hydration policy [fix: missing-from-all, indexer/AV storms]:** `GetFileData` receives `TriggeringProcessId` and `TriggeringProcessImageFileName`. Every hydration is counted per process image (metric and `asc debug hydrations`). `asc.vfs.denyHydration` (default `SearchProtocolHost.exe;SearchIndexer.exe;SearchFilterHost.exe`) returns `STATUS_CANNOT_ACCESS`-equivalent (`ERROR_ACCESS_DENIED`) for those images. The installer adds Defender exclusions for `$ASC_HOME`, and `asc doctor` checks Defender exclusions and Dev Drive status (Defender is ProjFS-aware and does not hydrate placeholders it does not open, which spike S1 measures). Directory enumeration and `ReadDirectoryChangesW` watchers (VS Code, turbo, vite) never hydrate. The documented VS Code settings exclude `node_modules`, `dist` and `.turbo` from search and watching.
   - **5a. Language servers [audit: ls-crawl-hydration]:** tsserver, rust-analyzer, Pylance/pyright and OmniSharp are expected to hydrate the source files of the projects they open. ASC's HEAD is 119 MiB, within `hydrationBudget`. To keep this fast, `asc clone`/`worktree add` start a background `asc prefetch` of the `.asc/prefetch` globs (default: tracked `**/*.{ts,tsx,rs,py,cs,toml,json}` outside redirections). It fills the shared machine cache only and creates no placeholders, so language-server hydration is a local copy. Dehydration (item 9) skips files that any process read in the last 3 days and files open in any process. The documented editor settings scope language servers to the workspace (rust-analyzer `files.excludeDirs` for redirections, `python.analysis.exclude`, tsconfig `exclude`).
6. **Notifications** (registered for the root; redirected names and the `.git` gitfile are filtered, §10.5): `NEW_FILE_CREATED`, `FILE_OVERWRITTEN`, `PRE_DELETE`, `PRE_RENAME`, `FILE_RENAMED`, `HARDLINK_CREATED`, `FILE_HANDLE_CLOSED_FILE_MODIFIED`, `FILE_HANDLE_CLOSED_FILE_DELETED` and `FILE_PRE_CONVERT_TO_FULL` append journal hints. They are **hints only**.
   - **Directory renames [fix: missing-from-all]:** on `PRE_RENAME` of a directory, the provider hydrates the whole subtree under the source first, so every descendant is a real file before the rename proceeds. It then journals `dir-renamed-from <src>` and `dir-renamed-to <dst>`. Status treats every tracked path under `src` (from the base tree) as deleted and walks `dst` on disk as new or untracked files. Directory renames are rare, so the hydration cost is accepted.
   - **Pre-hydration threading [audit: projfs-dir-tombstone-reconcile]:** pre-hydration runs on the provider's worker pool, not on the notification thread. The `PRE_RENAME` callback returns only after the pool finishes, and the pool keeps ≥ 2 threads free so callbacks never wait on a callback thread. If hydration fails (offline), the rename is denied with `STATUS_ACCESS_DENIED`, and the daemon log names the missing object.
7. **Reconcile on start [graft: performance]:** for each workspace, enumerate only on-disk (non-virtual) entries, never descending into redirected names (§10.5). ProjFS keeps placeholders, hydrated, full and tombstone entries as real directory entries; a recursive walk that skips virtual-only directories is bounded by what was materialised. Each path's state comes from `PrjGetOnDiskFileState`: Full or DirtyPlaceholder → journal `modified`; Tombstone → journal `deleted`; full files not in the tree → `created`. A directory tombstone expands to `deleted` for every tracked path under it in the base tree (metadata only). Then, if a `switch_intent` exists, the switch is re-run (§10.2.8).
8. **Switch, reset --hard, restore** (the daemon executes `Switch`):
   1. Persist `switch_intent {from_root, to_root, to_head, pending = changed paths}` **[graft: performance]**.
   2. Compute the work set `W = leafdiff(from_root, to_root)` (from metadata, plus directory-level changes) `∪`, in FORCE mode and for restore, the dirty set `D = journal ∪ staged ∪ reconcile results`, limited to the command's pathspec. `reset --hard`, `restore --worktree`, `checkout [<rev>] -- <paths>` and `switch -f` run in FORCE mode over `W`, even when `from_root == to_root` **[audit: projfs-reset-restore-dirty-set]**.
   3. Apply git's switch rules to dirty paths (journal ∪ staged): if the target changes a dirty path, refuse (SAFE), three-way merge the content (MERGE, `--merge`), or overwrite (FORCE, `-f`/`reset --hard`).
   4. For each path in `W`, in parallel batches:
      - virtual (never materialised) → nothing on disk; enumeration serves the new tree after the base changes;
      - SAFE/MERGE mode, clean placeholder or hydrated → `PrjUpdateFileIfNeeded(new placeholder info, PRJ_UPDATE_ALLOW_DIRTY_METADATA | PRJ_UPDATE_ALLOW_READ_ONLY)` (never `ALLOW_DIRTY_DATA`); dirty paths were handled by step 3;
      - FORCE mode, placeholder/hydrated/DirtyPlaceholder/Full whose target entry exists → `PrjDeleteFile(path, PRJ_UPDATE_ALLOW_DIRTY_METADATA | PRJ_UPDATE_ALLOW_DIRTY_DATA | PRJ_UPDATE_ALLOW_READ_ONLY)`, so the path becomes virtual again and enumerates from the target tree;
      - the target deletes the path → `PrjDeleteFile` with the same flags as the mode's update;
      - **tombstone** (local deletion) in `W` in FORCE mode, or a tombstone whose target entry is absent → `PrjDeleteFile(path, PRJ_UPDATE_ALLOW_TOMBSTONE)`, so the path becomes virtual (restored) or stays absent. In SAFE mode a tombstone that the target changes is a dirty path (git semantics) **[fix: missing-from-all, tombstones]**;
      - untracked Full files are never touched (git semantics; `asc clean` removes them).
   4a. **Watcher visibility [audit: watchers-virtual-switch]:** for every path the switch ADDS whose parent directory is on disk (placeholder or full directory), the daemon opens the path with `CreateFileW(…, 0 /*no access*/, FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)` and closes it. ProjFS then creates the placeholder (no hydration), and `ReadDirectoryChangesW` reports `FILE_ACTION_ADDED`. Updated and deleted on-disk placeholders already raise events through `PrjUpdateFileIfNeeded`/`PrjDeleteFile`. Paths under never-enumerated virtual directories need no event, because no watcher has seen them.
   5. **Files held open** (update fails with `ERROR_SHARING_VIOLATION` or the update-failure reason `PRJ_UPDATE_FAILURE_CAUSE_*`): retry 3 times at 100/300/900 ms, then leave the path in `switch_intent.pending` **[graft: performance]**. The switch completes. Pending paths get index entries for the target oid with stat values that cannot match, so turbo and asc status hash them. `asc status` shows them as `not updated (in use by <process>)`, and the daemon retries pending paths every 5 s until done, then clears them.
   6. Bump the epoch and record `entry_times` (§10.1). Rewrite the index and `HEAD` (holding `index.lock` from step 1), clear the negative path cache (`PrjClearNegativePathCache`), and delete the intent if nothing is pending.

   A crash at any step: on restart the intent is re-run idempotently (`PrjUpdateFileIfNeeded` to the target version is idempotent), then the index and HEAD are rewritten.
9. **Dehydration:** clean hydrated placeholders in a workspace are re-virtualised with `PrjDeleteFile(PRJ_UPDATE_ALLOW_READ_ONLY)` (only clean `HydratedPlaceholder` state that no process has open is eligible) when either: no process has read them for 3 days (from the hydration log, by process), or the workspace's hydrated bytes exceed `asc.vfs.hydrationBudget` (default 500 MiB), least recently hydrated first **[graft: performance]**. It runs during idle (no notifications for 60 s) and on demand with `asc sparse dehydrate <pathspec>`.
10. **Long paths:** all internal paths use `\\?\` verbatim form. `core.longpaths=true` is set. `asc doctor` checks `HKLM\SYSTEM\CurrentControlSet\Control\FileSystem\LongPathsEnabled=1` **[fix: missing-from-all]**.

### 10.3 Linux: FUSE (`asc-vfs::fuse`)

- `fuser` 0.18, `default_permissions`. **Inode table persisted** in `state.db.inodes` (allocated sequentially, never reused while the workspace exists), so inode numbers are stable across restarts and the git index's ino matches **[graft: performance] [fix J1]**.
- `getattr` returns tree metadata, with times from §10.1 and mode from `Kind`. The kernel caches attributes and entries with a 1 h TTL. Switch invalidates changed entries (`notify_inval_entry`, `notify_inval_inode`).
- **Watchers [audit: watchers-virtual-switch]:** inotify does not report changes the FUSE server makes. In FUSE virtual mode, `asc switch`/`pull`/`reset` print `note: file watchers do not see this switch; restart watch processes or use polling` when a watch-capable process (an inotify user on the mount, from `/proc/*/fdinfo`) is detected. Plain mode has no such limitation. This is a documented limitation of Linux virtual mode.
- Opening a clean file read-only uses **FUSE passthrough** (kernel ≥ 6.9) to the `cache/f/<id>` file, which gives zero copies and zero extra disk. Without passthrough it reads through.
- Open-for-write, create, truncate, unlink and rename copy up into `wc/<ws>/overlay/` (the overlay holds modified files and records deletions). Every write passes through the daemon, so the journal is exact and needs no reconcile.
- Redirected names are served as symlinks to `$ASC_HOME/redirect/<ws>/<path>`, created on first lookup.
- CI virtual mode (later, after measurement): the daemon runs on the runner host, and the job sees the mount through an `rshared` bind mount.

### 10.4 Plain mode (`asc-wc::plain`; all OSes; CI default; macOS v1)

- Materialise the tree, or a cone-only sparse set (`asc sparse set <dir>…`; non-materialised entries carry skip-worktree in the index), from the cache using `reflink-copy` (APFS `clonefile`, btrfs/XFS, ReFS Dev Drive) or a plain copy. **Never hardlinks**, so an editor can never corrupt the shared cache.
- Status is a git-style stat check of index entries against the files (4.5k files take milliseconds), kept warm by `notify` while the daemon runs. Untracked files come from a walk with the ignore rules of §2.4.2.
- Switch updates only changed files: write to a temp file in the same directory, then rename. Mtimes are real.

### 10.5 Redirections

`.asc/redirections` (checked in) lists directory names. ASC's default is `node_modules`, `dist`, `.turbo`, `.next`, `target`, `.venv`, `__pycache__`.

- **ProjFS [audit: redirection-mechanics]:** redirected names are ordinary NTFS directories inside the root (not in the tree, so never virtual). At mount the provider registers `PRJ_NOTIFY_NONE` mappings for every existing `<dir>/<name>` that matches `.asc/redirections`; mappings are fixed path prefixes, so the notification callback also drops, before any other work, any path that has a component in the redirection list, which covers directories created after mount. Reconcile (§10.2.7), status untracked walks and `asc clean` never descend into redirected names. bun's hardlinks need the **workspace root** and `BUN_INSTALL_CACHE_DIR` on the same volume; `asc worktree add`/`clone` warn when they differ, and `asc doctor` checks it. No junctions are used for redirections **[fix: missing-from-all]**.
- **FUSE:** symlinks to `$ASC_HOME/redirect/<ws>/<path>` on the same filesystem as `$ASC_HOME`.
- **Plain:** ordinary directories, excluded from status by the ignore rules (§2.4.2).

### 10.6 Submodules (gitlinks)

- Gitlinks are kept losslessly (`Kind::Gitlink`, `Change::Gitlink`).
- **Read-only composition [graft: performance; decision per judge 2]:** each gitlink's repo is configured with `asc.submodule.<path>.repo` (default derived from `.gitmodules` `url`, e.g. `../soma.git` → `asc/soma`). The VFS and plain mode project the nested repo's tree at the pinned commit (git oid → changeset through the nested repo's git map) at that path, from the same shared cache. The composed subtree is **read-only**: writes fail with `EACCES`/`ERROR_ACCESS_DENIED`. `asc status` ignores it. The synthetic index keeps the gitlink entry (mode 160000), as git does for an initialised submodule.
- To change the sdk, work in a separate workspace of `asc/soma` and update the pin with `asc submodule set-pin <path> <rev>` (writes a `Change::Gitlink`). `asc submodule status` lists pins. `asc submodule update` is a no-op with a notice.
- This removes the 27 GB of per-worktree submodule git dirs and `submodules: recursive` from CI without merging repositories. **Folding `soma` into `dds` remains a user decision** (§19). If chosen, it is one changeset, and composition stops mattering.

---

## 11. CLI (`asc`)

### 11.1 Principles

- Same verbs, flags, revision syntax, exit codes (0, 1 for diff-like or "nothing to commit", 128 fatal, 129 usage) and output formats as git 2.55: `--porcelain[=v1|v2]`, `-z`, `--format`/`--pretty`, `--name-status`, `--stat`. Native `log`/`show`/`for-each-ref`/`branch --format`/`tag --format` implement git 2.55's **complete** pretty-format placeholder set and ref-filter atom set (including `%aN %aE %aI %cI %ai %ci %x<hh> %gs %gd %gD %m %G? %GS %(trailers) %(upstream[:short|:track|:remotename]) %(push[:track]) %(HEAD) %(*objectname) %(committerdate:unix) %(contents:subject)`) **[audit: unsupported-flags-delegate]**.
- Which invocations are native, and what happens to the rest, is §11.5. Messages are §11.6.
- **Environment and global options [audit: git-env-vars].** asc honours, with git's semantics:
  - `GIT_DIR`, `GIT_WORK_TREE`, `GIT_COMMON_DIR`, `GIT_INDEX_FILE` (all staging reads and writes, including hooks; §9.5), `GIT_CEILING_DIRECTORIES`, `GIT_DISCOVERY_ACROSS_FILESYSTEM`;
  - `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_<n>`/`GIT_CONFIG_VALUE_<n>`, `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`, `GIT_CONFIG_NOSYSTEM`. asc exports its own `-c` values to children (hooks, editors) the same way;
  - `GIT_AUTHOR_{NAME,EMAIL,DATE}`, `GIT_COMMITTER_{NAME,EMAIL,DATE}`, `GIT_EDITOR`, `GIT_SEQUENCE_EDITOR`, `GIT_PAGER`, `GIT_MERGE_AUTOEDIT`, `GIT_REFLOG_ACTION`;
  - `GIT_OPTIONAL_LOCKS=0` / `--no-optional-locks`: no opportunistic index refresh or write, and no `index.lock`;
  - `GIT_ASKPASS`, `SSH_ASKPASS`, `core.askPass`, `GIT_TERMINAL_PROMPT`, `GIT_SSH`, `GIT_SSH_COMMAND`, passed through to `git credential` and to asc's own token prompt;
  - `GIT_LITERAL_PATHSPECS`, `GIT_GLOB_PATHSPECS`, `GIT_NOGLOB_PATHSPECS`, `GIT_ICASE_PATHSPECS`, and the matching global flags;
  - `GIT_TRACE`, `GIT_TRACE2*` (asc writes its own trace in git's format to the same target), `NO_COLOR`.

  Messages are always git's English text (§11.6); `LANG`/`LC_ALL` are accepted and ignored. Global options: `-C`, `-c`, `--config-env`, `--git-dir`, `--work-tree` (for all commands, read and write), `--no-pager`, `-p/--paginate`, `--no-optional-locks`, `--literal-pathspecs`, `--glob-pathspecs`, `--noglob-pathspecs`, `--icase-pathspecs`, `--no-replace-objects`, `--exec-path` (prints real git's), `--version`, `-h/--help`. Plus `alias.*` expansion (including `!shell` aliases), `git-<name>` and `asc-<name>` external commands on `PATH` in the §11.4 order **[audit: shim-asc-verbs-shadow-aliases-and-git-dash-commands]**, git's pager rules (`core.pager`, `GIT_PAGER`, `PAGER`, `less -FRX`) and `color.ui`.
- Config comes from git's files (`gix-config`): system, `~/.gitconfig`, XDG, and the workspace's common `config` (§9.4), with `includeIf` evaluated as §9.4 says. asc keys live under `asc.*` (stack keys under `asc.stack.*`: `trunk` (multi-valued, default the default branch), `immutable`, `exclude`, `branchTemplate`, `submit.draft`; and `asc.opLog`, `asc.opLog.maxDays`, `asc.opLog.maxOps`; §20.3; branchless keys `asc.branchless`, `asc.cloud*`, `asc.draft.hiddenDays`, `asc.push.review`; §21.6.1). Credentials use `git credential fill/approve/reject` through `RealGit` (Git Credential Manager on Windows).
- **Ids in output are git SHA-1s by default** in every mode (local-only, bucket, git-remote, server) **[audit: display-ids, display-ids-contradiction]**. Abbreviation follows git (`core.abbrev`; default auto, at least 7, extended until unique). The SHA comes from `gitcommit.v1` (§5), which exists for every changeset in every mode. `asc.displayIds = git (default) | asc | both` is a per-user opt-in that applies only when invoked as `asc`, and only to human-readable output; it never applies to `--porcelain`, `-z`, `--format`/`--pretty`, plumbing commands, or argv[0] = `git`. `%(ascid)`, `%C` (asc id placeholder) and `rev-parse --asc` stay available as additive features. Input accepts both kinds of id (§2.1).
- `asc help git` prints the git→asc cheat sheet (§11.3). `asc help <cmd>` shows asc-specific notes and then git's manual.

### 11.2 Command map

Where: **L** local; **L+f** local, fetching missing objects; **S** server RPC when the remote is a server (otherwise the same engine code runs locally).

| git command | asc behaviour | Where |
|---|---|---|
| `init [dir] [-b <b>] [--store <url> [--refs local]]` | local-only repo plus a plain workspace; default branch per §6.5. `--store` puts objects (and BucketRefs once U30 ships) in a bucket | L |
| `clone <url> [dir]` | §9.3. `--mode virtual\|plain` (default virtual where available), `-b`, `--sparse`, `--no-checkout`, `--reference`, `--origin`, `--seed`. `--depth`/`--filter` are accepted (always lazy) | L+f |
| `worktree add\|list\|remove\|prune\|lock\|unlock\|move` | §9.3; `--ephemeral --ttl` | L |
| `status` | `-s`, `-b`, `--porcelain[=v2]`, `-z`, `-u[no\|normal\|all]`, `--ignored`, `--ahead-behind` | L |
| `add`, `rm`, `mv` | edit staging (§9.5; `-p`, `-u`, `-A`, `-N`, `--cached`, `-f`, `-n`); `mv` records `copy_from` | L |
| `restore`, `reset` | `--staged`, `--worktree`, `--source`; `reset --soft\|--mixed\|--hard [<rev>] [-- paths]`; virtual uses the daemon switch | L+f |
| `commit` | §9.7. `-m`, `-F`, `-a`, `--amend`, `--fixup`, `--squash`, `--no-verify`, `--allow-empty`, `-S`, `--author`, `--date`, `-v`, `-e`/`--no-edit` | L |
| `switch`, `checkout` | `-c`/`-C`, `--detach`, `--orphan`, `--merge`, `-f`, `-` ; `checkout <rev> -- <paths>` restores | L+f |
| `branch` | `-d`/`-D`, `-m`/`-M`, `-c`, `-u`/`--set-upstream-to`, `--unset-upstream`, `-r`, `-a`, `-v`/`-vv`, `--list <pat>`, `--contains`, `--merged`, `--format` | L |
| `tag` | lightweight and annotated (`-a`, `-m`, `-s`, `-d`, `-l`, `-f`, `-n`) | L |
| `fetch` | refs plus changesets within the history window only (§6.3). `--prune`, `--all`, `--tags`, `--deepen=<n>`, `--unshallow`, `--prefetch <pathspec>`. In a branchless repo the daemon also pulls cloud heads, best-effort, and a failure never changes fetch's output or exit code (§21.4.2) | L+f |
| `pull` | fetch then merge or rebase per `pull.rebase`/`pull.ff`; `--rebase`, `--ff-only`, `--no-rebase` | L+f |
| `push` | §6.3/§8.3; `-u`, `--force`, `--force-with-lease[=ref:expect]`, `--delete`, `--tags`, `--all`, `-o`, `--dry-run`, `--porcelain`; asc-only `--land` (= `-o land`, §8.5) and `--no-ref` (§8.7); `refs/for/<b>` (AGit, §13.5). Output and rejections are git's, byte for byte. In a branchless repo (§21.4.2) a non-shared branch becomes the pusher's user ref and its stack is submitted as draft PRs; `--all` creates user refs only; `--mirror` is refused | S |
| `merge` | local engine; `--no-ff`, `--ff-only`, `--squash`, `--abort`, `--continue`, `-m`, `--no-commit`, `-X ours\|theirs`, `--conflict` style via `merge.conflictStyle` | L+f |
| `rebase` | local engine; `--onto`, `--continue`, `--abort`, `--skip`, `-i` (git's todo format and editor), `--autosquash`, `--exec`, `--update-refs` | L+f |
| `cherry-pick`, `revert` | local engine; `-n`, `-x`, `-m <n>`, `--continue/--abort/--skip` | L+f |
| `stash` | `push [-u] [-m] [-- paths]`, `pop`, `apply`, `list`, `show [-p]`, `drop`, `clear`, `branch`. git's stash structure (W with parents [HEAD, I], optional U) on `refs/stash`, plus reflog | L |
| `log` | `--oneline`, `--graph`, `--format`, `-p`, `--stat`, `--name-status`, `--follow`, `-- <path>`, `--since`/`--until`, `--author`, `--grep`, `-n`, `--first-parent`, `--all`, `--decorate`, `--reverse`, ranges | L / S |
| `show` | `<rev>`, `<rev>:<path>`, `--stat`, `--name-only`, `--format` | L+f / S |
| `diff` | worktree/index/HEAD (L); `<a> <b>`, `<a>...<b>` (L, or S with `--remote <repo>` and no checkout); all formats; `--cached`, `-M`, `--no-index` (native), `--classify` (asc-only, §8.10) | L+f / S |
| `blame` | `-L`, `--porcelain`, `--incremental`, `-w`, `-M`, `--reverse` (S when a server remote exists, else local). Dirty files are blamed against the working copy locally | S / L |
| `grep` | clean files: server `Grep` at base (no hydration); dirty files: local. Local-only repos: local over the tree | S+L |
| `remote` | `add`, `remove`, `rename`, `set-url`, `get-url`, `-v`, `show`, `prune` | L |
| `config` | git config files; `--global`, `--local`, `--worktree`, `--get`, `--list`, `--unset` | L |
| `rev-parse`, `rev-list`, `merge-base`, `show-ref`, `for-each-ref`, `symbolic-ref`, `update-ref`, `describe` | native. `rev-parse --git <rev>` / `--asc <rev>` print either id | L / S |
| `ls-files`, `ls-tree`, `cat-file` (incl. `--batch`, `--batch-check`), `diff-tree`, `diff-index`, `diff-files`, `hash-object`, `check-ignore` (`-v -z --stdin -n --no-index`), `check-attr` (`-a -z --stdin`) | native, from metadata and staging; `ls-tree --format` supports `%(objectname)` (git) and `%(ascid)` | L+f |
| `update-index`, `read-tree`, `write-tree`, `commit-tree`, `mktree`, `merge-tree`, `stash create\|store`, `apply --cached`, `interpret-trailers`, `var`, `check-ref-format`, `ls-remote` | native, for the forms in the native table (§11.5) that hooks, lint-staged, Claude Code and repo scripts use | L |
| `reflog` | `state.db` ref log; `--remote` shows the server log | L |
| `clean` | `-n`, `-f`, `-d`, `-x`, `-X`; virtual mode walks only materialised directories | L |
| `bisect` | `start/good/bad/skip/reset/run/log`; midpoints from the graph; the working copy switches each step | L+f |
| `archive` | server `Archive`, or local streaming | S / L |
| `sparse-checkout` | `set`/`add`/`list`/`disable` = cone in plain mode; in virtual mode = the prefetch profile | L |
| `submodule` | `status`, `set-pin`; `update`/`init` are no-ops with a notice (§10.6) | L |
| `gc`, `maintenance`, `prune`, `fsck`, `count-objects`, `repack` | `gc` = cache eviction plus dehydration; `fsck` verifies `store/`/`cache/` hashes and workspaces; `repack`/`prune` print "not needed in asc" and exit 0 | L |
| `format-patch`, `apply`, `am` | local engine | L |
| `shortlog`, `name-rev`, `whatchanged`, `range-diff`, `cherry`, `notes show` | native in a later unit; until then git-style usage error (§11.5) | L |
| `filter-branch`, `filter-repo`, `replace`, `svn`, `p4`, `send-email`, `daemon`, `instaweb`, `lfs`, `notes` (write) | not available in an asc workspace: `fatal: asc: '<cmd>' is not supported in an asc workspace` (exit 129). Outside asc workspaces the shim runs real git (§11.4). `lfs`: large files are native | – |

Until a command's unit lands, invoking it fails with exit 129 and `fatal: asc: '<cmd>' is not supported yet` **[audit: missing-cli-units]**.

asc-only commands (no git equivalent, so they never change git behaviour): `asc daemon [start|stop|status]`, `asc serve`, `asc import`, `asc export`, `asc auth login|status|logout`, `asc doctor [--fix] [--store <url>] [--report-reclaimable]`, `asc prefetch`, `asc sparse dehydrate`, `asc debug stats|hydrations|trace`, `asc ci checkout|affected|changed|warm|cache|artifact`, `asc affected`, `asc projects`, `asc graph` (§12A.9), `asc landed` (§8.9), `asc api [--stdio]` (§7, §24.2), `asc watch` (§7), `asc search`, `asc sym` and `asc semantic` (§23.4.2), `asc codeintel status|worker|enable-embed` (§23.4.2), `asc mcp [--read-only | --write[=local|remote]] [--toolsets <list>] [--tools <list>] [--worktree <path>]` and `asc mcp install [--scope project|user] [--no-forgejo]` (§22.8), `asc install-shim` (§11.4), `asc migrate ignore` (§2.4.2), `asc migrate branchless` (§21.6.2), `asc hide`, `asc unhide`, `asc cloud status|sync|join|list|release|purge` and `asc sl --cloud --user` (§21.3), `asc worktree convert --plain` (§9.1), `asc upgrade`, `asc internal …` (hooks, Claude Code hooks), and the stack verbs of §20.7: `asc create`, `modify`, `absorb`, `restack`, `continue`, `abort`, `sync`, `submit`, `sl`/`smartlog`, `up`/`down`/`top`/`bottom`/`next`/`prev`, `reword`, `uncommit`, `unamend`, `abandon`, `change-id`, `split`, `squash`, `fold`, `move`, `reorder`, `stack rename|delete|pop|get|freeze|unfreeze|trunk|info|parent|children|merge|exec`, `undo`, `redo`, `op log|show|restore`, `evolog`, `interdiff`, `share`. `asc stack merge` (asks the server to land a stack prefix) is distinct from `asc landed` (§8.9), which only reports.

asc-only flags for the plugin API [audit: plugin-api-undefined]: `status|diff|log|blame --json` print the matching `Ws*Response` in the asc JSON profile (§22.4.2; one object per stream message, as JSON lines); `asc watch [--since <token>] --json` streams `WatchEvent`s as JSON lines. For the IDE bridge (§24.2, §24.8.2 A5, A7, A8) [audit: ide-plugins-folded]: `asc reorder --todo <file>` applies a `StackTodo` as one operation; `asc split --hunks <spec>` splits non-interactively from a `SplitSpec` (the interactive `--by-hunk` is unchanged); `restore`, `switch`, `cherry-pick`, `revert`, `clone`, `worktree add|remove|list`, `stash`, `fetch`, `pull` and `push` gain `--json` forms that print a typed `OpResult` (M02); `ASC_NO_ALIASES=1` disables `alias.*` and external-command resolution; every verb the bridge runs accepts `--end-of-options`.

### 11.3 Cheat sheet (`asc help git`, also `docs/CHEATSHEET.md`)

The rule is to keep using your git commands: with the shim installed (§11.4), `git` *is* asc inside an asc workspace and real git elsewhere; `asc <cmd>` is the same thing. Differences: `clone` and `worktree add` are instant and virtual; `fetch` downloads no files; `git push --land` (or `-o land`) rebases on the server when the paths don't conflict; a plain push behaves exactly as git's; `submodule update` is unnecessary; `lfs` is unnecessary; IDE git panels are replaced by ASC's asc plugins (§24). In a branchless repo (the default) a branch is a local label: new commits upload in the background, `git push origin feat` also submits the stack as draft changes, and `asc hide` hides a draft (§21).

### 11.4 Installed as `git` (the shim) [audit: git-shim-argv0, git-shim-install-as-git]

- **Dispatch [audit: windows-shim-path-precedence-and-location]:** the `asc` binary MUST behave as the shim when argv[0]'s file stem is `git` (case-insensitive; `.exe` ignored). `asc install-shim [--dir <d>]` creates `git`/`git.exe` in `<prefix>/<ver>/shim/` (a hardlink to `asc.exe` on the same volume on Windows, a symlink on Unix), and prints the PATH line. PATH entries point at `<prefix>/current/shim`. `--dir <d>` must be on the binary's volume on Windows, else `fatal: asc: --dir must be on the same volume as asc.exe`. The same directory is the private shim dir that asc puts first on its children's PATH (§9.4.1). The MSI and `install.sh` offer user-PATH installation as an option, "Use asc for git commands", which is on by default for the pilot cohorts (§18).
- **Real git resolution (MUST, all modes, not only the shim):** every place where asc runs git uses `RealGit::path()`/`RealGit::command()` (crate `asc-git::realgit`), never `Command::new("git")`; a CI grep check (`build/lint/no-raw-git.sh`) enforces this for non-test code. Test-only code in crates below `asc-git` (asc-core, asc-store, asc-engine, asc-graph) may build golden fixtures with a raw `git` child; such a line carries the marker `lint:allow-raw-git` **[audit: no-raw-git-lint-fails-built-code]**. Resolution order:
  1. env `ASC_REAL_GIT`;
  2. global config `asc.realGit`;
  3. the first `git[.exe]` on `PATH` whose file identity (volume and file id / dev and ino) differs from the running binary;
  4. on Windows, `%ProgramFiles%\Git\cmd\git.exe`.

  The result is cached in `$ASC_HOME/run/real-git`, validated by the target's mtime. If nothing is found: `fatal: asc: real git not found; set asc.realGit`, exit 128. Real git ≥ 2.40 is a required dependency. **Loop guard:** the shim sets `ASC_SHIM_DEPTH=<n+1>` for its children and fails with exit 128 when n > 4.
- **Repository classification:** the shim runs git's discovery itself: `-C`, `--git-dir`, `--work-tree`, `GIT_DIR`, `GIT_WORK_TREE`, `GIT_CEILING_DIRECTORIES`, `GIT_DISCOVERY_ACROSS_FILESYSTEM`, and walking up to a `.git` file or directory. The result is an **asc workspace** if and only if the resolved gitdir contains the marker file `asc-workspace` (§9.4). Otherwise the shim runs real git with identical argv[1..], env, cwd and stdio: `execv` on Unix; on Windows `CreateProcess` with inherited handles, forwarding Ctrl-C/Ctrl-Break and returning the child's exit code. Discovery MUST NOT open `state.db` or contact the daemon. **Budget:** fall-through overhead p95 ≤ 5 ms (Windows) / ≤ 2 ms (Linux), measured by O05.
- **Outside any repository:**
  - `--version` prints `git version <real git version>.asc.<asc semver>` (for example `git version 2.55.0.asc.1.2.0`), which still matches the `^git version (\d+\.\d+\.\d+)` parsers tools use. `asc --version` prints `asc <semver> (git <real version>)`.
  - `clone <url>` becomes `asc clone` when the URL is `asc+…`, `s3://`, or `file://` to a bucket layout, or when discovery (§6.2) finds an asc server for the URL. Otherwise it is real git.
  - `init` is real git, except that the shim adds `-c init.defaultBranch=master` when the user has not set `init.defaultBranch` (§6.5); the same applies to real git `clone`.
  - **[audit: compat-fixture-routing]** With global config `asc.shim.init = asc` (default `git`), `init` and `clone` route to asc instead of real git. Invoked as `git`, they stay git-identical: `init` writes no `.ascignore` (argv[0] = `git` is never given asc-only additions, as for display ids, §11.1).
  - Everything else (`config --global`, `ls-remote`, `credential`, `help`, …) is real git.
- **Repository-independent invocations [audit: repo-independent-commands-routing]** route by their target, not by cwd: `clone` (target = the destination directory), `init <dir>` with a directory other than the current workspace root, `ls-remote <url>` (a URL or path, not a configured remote name), `config --global|--system|--file <f>` (with `<f>` outside the workspace), `credential*`, `help`, `version`/`--version`, and `archive --remote`. They follow the "Outside any repository" rules even when cwd is inside an asc workspace. A real-git clone or init inside a workspace creates a nested git repository: the parent's status reports it as `?? <dir>/` (§9.3 nested rule), and discovery inside it finds a plain `.git`, so it is served by real git. `asc init`/`asc clone` (invoked as `asc`) always create asc repositories.
- **Inside an asc workspace** every git builtin is asc (§11.5). Resolution of `<name>` follows git's order **[audit: shim-verb-precedence, shim-asc-verbs-shadow-aliases-and-git-dash-commands]**:
  - *argv[0] = `git`:* (1) a git 2.55 builtin name: the native table, else the §11.2 unsupported error. `daemon` is git's builtin, so `git daemon` is unsupported, and asc's daemon is reachable only as `asc daemon`. (2) `alias.<name>`. (3) an executable `git-<name>` on PATH or in real git's exec-path, run with the §9.4.1 child PATH. (4) an asc-only verb (§11.2: `affected`, `projects`, `graph`, `landed`, `watch`, the stack verbs, …). (5) git's `git: '<name>' is not a git command. See 'git --help'.` (exit 1).
  - *argv[0] = `asc`:* (1) git builtins, (2) asc-only verbs, (3) `alias.<name>`, (4) `asc-<name>` then `git-<name>` on PATH.
  - So under `git`, an asc-only verb (a stack verb included) runs only where git would print "not a git command": a user alias (for example `alias.graph = log --graph`) or a `git-<verb>` on PATH (git-extras' `git undo`/`git sync`, gitext's `git stack`, `git absorb`) wins (§20.1 D5). The verb match is exact and happens before `help.autocorrect`: autocorrect never runs for a name that is an asc-only verb, and asc-only verbs are never autocorrect candidates, so a typo of a git command is corrected as in git.
- **Install surfaces (documented in O04, checked by `asc doctor`):**
  - *Windows:* Git for Windows adds `Git\cmd` to the **machine** PATH, which Windows puts before the user PATH. The elevated MSI therefore inserts `<prefix>\current\shim` into the machine PATH immediately before the first Git for Windows entry. A non-elevated install cannot shadow git and says so.
  - *Git Bash* (which Claude Code uses on Windows, and which puts `/mingw64/bin` first): the installer adds `export PATH="<shimdir>:$PATH"` to `~/.bashrc` inside a `# asc shim` marked block.
  - `asc doctor` resolves `git` in a freshly spawned cmd, PowerShell and Git Bash process (and the turbo environment) and reports any surface where it is not the shim. Children that asc spawns need none of this (§9.4.1).
  - IDE git integrations are not a target (§0 decision 6); when an editor's git path points at the shim, it gets whatever the native table covers.

### 11.5 Routing: native table [audit: unsupported-flags-delegate]

- asc serves an invocation natively when the whole invocation (command plus every flag, format placeholder and ref-filter atom) is in the **native table** `crates/asc/src/native.rs` (`NativeSpec { cmd, flags, placeholders }`, `route(argv) -> Native | Unsupported(reason)`). Every table entry MUST be covered by at least one passing compatibility case (§17.1).
- In an asc workspace there is no fall-through to real git: real git cannot operate there (§9.6). An invocation outside the table fails before doing anything, with `fatal: asc: option '<flag>' is not supported` (or `'<cmd>' is not supported`) and exit 129. Flags are never ignored.
- **Coverage is driven by the users in scope:** humans, repo scripts, Claude Code, husky, lint-staged, commitlint and turbo. Their recorded traces (§17.1) define the required table. In particular the table covers lint-staged's `stash create|store`, `diff --binary --output --src-prefix --submodule=short`, `apply --recount --unidiff-zero --whitespace=nowarn --3way` and `GIT_INDEX_FILE` flow; Claude Code's status, diff, log, worktree, branch and push invocations; and every invocation turbo makes (spike S4).
- History rewrites that need real git (`filter-branch`, `filter-repo`, `replace`) are unsupported in asc workspaces. `repack`, `prune` and `gc` are native (asc semantics) and exit 0.
- asc-only behaviour is reached only through new subcommands or flags (`--land`, `--no-ref`, `--classify`, `--asc`, `%(ascid)`), never by changing an existing command's behaviour.
- Stacks add no git flag: real git echoes unknown `--options` back from `rev-parse`, so `rev-parse --change-id` is deliberately not added; the change id is read with `asc change-id <rev>` (§20.2). Op-log recording and `ChangeMeta` writing are invisible in git output (§20.3, §17.1).

### 11.6 Messages [audit: error-wording]

- For every condition that git also has, asc MUST print git 2.55's English (`LC_ALL=C`) message byte for byte on the same stream (stdout or stderr), with the same prefix (`fatal: `, `error: `, `warning: `, `hint: `), the same `advice.*` gating and the same exit code. This covers, among others: not a repository; pathspec did not match; local changes would be overwritten (checkout, merge, pull); the merge/rebase/cherry-pick `CONFLICT (…)` and `Automatic merge failed…` lines; the ahead/behind/diverged lines of `status` and `checkout` (`Your branch is ahead of 'origin/master' by 1 commit.`); no tracking information; `Switched to branch` and `Switched to a new branch`; and the branch-in-use-by-worktree error.
- Message strings live in `crates/asc/src/msg.rs` (`Msg` enum). Each one cites the git source string it copies, and a test checks every string against the pinned git `po/git.pot` msgid. Hints name the program as the user invoked it (`git` or `asc`); the compat suite normalises only that word.
- `push` output is git's: `To <url>`, the ref-update summary lines, `! [rejected] <src> -> <dst> (fetch first|non-fast-forward|stale info)`, `error: failed to push some refs to '<url>'` with git's hints, and the `--porcelain` format.
- asc-only conditions use git's style with an `asc: ` tag after the prefix, for example `fatal: asc: offline (origin unreachable): log needs Log RPC`, and git's exit codes (128 fatal, 129 usage).

---

## 12. Turborepo and CI

### 12.1 Hashing without hydration

Turbo 2.10's hot path reads `.git/index` with `gix-index`, stat-compares each entry, and trusts the stored SHA-1 when the stat matches. The synthetic index carries the git oid and the exact stat the VFS reports (§9.4, §10.1), and no entry is racy. On a clean virtual workspace, `turbo run --dry=json` therefore reads no tracked files except manifests (which turbo parses) and produces the same hashes as a git clone. Turbo's other git calls (`rev-parse`, `ls-files --others`, `diff-tree --merge-base`, `show <base>:bun.lock`) reach asc through the shim (§11.4) and are answered natively from metadata. The only content fetched is `bun.lock` at the base, and only when it changed. Staged-but-uncommitted files are index entries (§9.4 renders set 0), so turbo hashes them exactly as for a git clone **[audit: turbo-index-reflects-head-breaks-staged-contract]**. ASC's comment-only `.gitattributes` should still be deleted (§12.5); attributes that turbo would apply are covered by §2.4.1.

### 12.2 Plan job (no checkout)

```yaml
plan:
  runs-on: docker
  steps:
    - run: asc ci changed --repo "$ASC_REPO" --base "$BASE" --head "$HEAD" --rules .asc/ci.toml >> "$GITHUB_OUTPUT"
    - run: asc ci affected --repo "$ASC_REPO" --base "$BASE" --head "$HEAD" --rules .asc/ci.toml >> "$GITHUB_OUTPUT"
```

- `--rules <path>` names a repository path, read with `ReadFile` at the merge base (no checkout). When the changed paths include it, every switch is true and `all=true` (§12A.10), so a PR cannot narrow its own CI by editing the rules. `--rules-file <local path>` reads a local file instead (tests, local use) **[audit: plan-job-rules-file-without-checkout]**.
- `asc ci changed`: one `Diff(NAMES, merge_base=true)` RPC, then glob rules from `.asc/ci.toml` (`[switches] rust = ["apps/analysis-pipeline/**", "**/*.rs", "**/Cargo.toml"]`, `docs_only = …`). It writes `key=value` lines, plus `semantic=<overall class>` (§8.10). This replaces Forgejo `paths:` filters and the docs-only short-circuit.
- `asc ci affected` **[audit: ci-plan-job-uses-turbo]** = `asc affected --repo R --format gha` (§12A.9). It makes one `Affected` RPC (§12A.8). There is no checkout, no sparse checkout, no turbo binary and no install.
  - It also evaluates `.asc/ci.toml` `[switches]` entries of the form `{ projects = ["<selector>", ...] }`. Such a switch is true when any affected project matches, for example `images = { projects = ["tag:image"] }` or `rust = { projects = ["eco:rust"] }`. Each switch is written as one more `key=value` line.
  - If the RPC fails, it writes `all=true`, sets every switch to true, and prints a warning.
  - Turbo stays the executor inside build jobs: `turbo run ... --affected` with `TURBO_SCM_BASE` (§12.3), or `--filter` taken from the plan's `turbo_filter`. asc plans; turbo executes.
- `.asc/ci.toml` schema: `[switches]` (globs or `{ projects = [...] }`) and `[outputs]` names. There is no `turbo`/`turbo_sha512` key.

### 12.3 Build/test jobs: `asc ci checkout`

```yaml
- uses: actions/checkout@v4     # aliased to asc/actions/checkout (§12.7); with: mode: plain (default) | virtual
  with: { base: "${{ github.base_ref || github.event.before }}" }
- run: bun install --frozen-lockfile
- run: turbo run build check-types test --affected
```

`asc ci checkout --repo R --rev <head> --base <base> --dir . [--mode plain] [--sparse …]`:

1. authenticates with the Actions task token (`GITHUB_TOKEN`);
2. resolves head and base, and `MergeBase`;
3. builds `Tiered<HostCacheRO, JobCache, Remote>`. **`HostCacheRO`** is the runner host's `/var/cache/asc/cache`, bind-mounted **read-only** into the job container. **`JobCache`** is `$RUNNER_TEMP/asc-cache`, writable and discarded after the job. Every read is BLAKE3-verified **[fix J1: CI cache poisoning]**;
4. materialises the tree (plain, reflink or copy), writes the synthetic `.git` (§9.4; index with real stat), registers `refs/remotes/origin/<base>` in the job's `state.db`, and puts the `git` shim first on the job's `PATH` so turbo's git calls are answered by asc. No `shallow` file is written. History beyond the job's local window is answered by the server (§6.3 history boundary), so `git log`, `git describe` and `rev-parse --is-shallow-repository` (`false`) behave as in a `fetch-depth: 0` clone, whatever `fetch-depth` is set to **[audit: ci-shallow-semantics]**;
5. exports `TURBO_SCM_BASE=<merge-base git sha>`, `TURBO_SCM_HEAD=HEAD`, `TURBO_API=<asc serve public_url>`, `TURBO_TOKEN=<task token>`, `TURBO_TEAM=<owner>.<repo>`, and `TURBO_CACHE=local:rw,remote:rw` (trusted) or `local:rw,remote:r` (PRs);
6. persists the job's tier stack in the repository's common config `$ASC_HOME/repos/<id>/.git/config` (§9.4; keys per §3.5) **[audit: ci-tier-stack-config-undefined-unowned]** (`asc.cache.dir = $RUNNER_TEMP/asc-cache`, `asc.cache.readOnly = /var/cache/asc/cache`, `remote.origin.ascServer`), and appends `ASC_HOME=$RUNNER_TEMP/asc-home` and `ASC_TOKEN=<task token>` to `$GITHUB_ENV` and the shim directory to `$GITHUB_PATH`. Later steps (turbo's `git show <base>:bun.lock`, `git diff-tree`, scripts) then use the same workspace, caches and credentials **[audit: ci-later-steps-asc-home-and-runner-mounts]**.

There is no submodule clone (sdk composed via the nested repo, §10.6) and no LFS (§2.5).

**Host cache warmer:** `asc ci warm --repo R --ref master` runs on each runner host as a systemd timer every minute, plus 0–30 s of random jitter per run, as a host-side user that owns `/var/cache/asc`. It fetches trunk's changesets, trees and files into the host cache. Only this trusted process writes the host cache. PR jobs never can.

**Seed snapshots (built in v1) [audit: 15-seed-snapshots, user decision efficiency]:**
- A seed is a zstd tar of a `CacheDir` subtree (`c/ t/ f/ l/ k/ d/`, files under their final content-addressed names) plus `seed.json { version: 1, repo, changeset, created }`. It holds the master tip's changeset history metadata within the default window (§6.3), all trees, and the hot blobs: every claimed manifest (§12A.2) plus the `.asc/prefetch` globs.
- The server's `seeds` task publishes a seed for each repo's default branch every hour, and when 50 new changesets have landed, at `seeds/<repo-slug>/<changeset>.tar.zst`, and updates `seeds/<repo-slug>/latest`. It keeps the last 3. `GET /seeds/<owner>/<repo>/latest` redirects to a presigned URL.
- `asc clone --seed <url>` (default: the discovery document's `seeds`), `asc ci warm --seed-from <url>` and a cold daemon cache unpack the seed into `cache/` before the first fetch, then fetch only the delta. Cache files are content-addressed and verified on read, so unpacking needs no index or validation pass, and a stale seed is still correct.

**Ignore files** follow §2.4.2 on CI as elsewhere.

**Virtual CI** (FUSE on the host, the job sees the mount through a bind mount) is built later, only if measurements show plain checkout (≈ 1–3 s for 119 MiB from a warm cache) is a bottleneck.

### 12.4 Turbo remote cache on the same store (`asc serve /v8`)

- Implements `GET /v8/artifacts/status`, `HEAD|GET|PUT /v8/artifacts/{hash}`, `POST /v8/artifacts` (batch query) and `POST /v8/artifacts/events`. Objects are stored at `turbo/<owner>.<repo>/<scope>/<hash>` with `.meta` JSON (`duration`, `tag`, `sha`, `dirty_hash`, `size`, `at`). `x-artifact-tag` (the HMAC signature) is stored opaquely and returned. ASC enables `remoteCache.signature: true`.
- **Scopes:** `trusted` and `pr-<n>`. Reads try the caller's scope, then `trusted`. Writes go only to the caller's scope.
  - Stage 2+ (fork authz available): the authz response says whether the token is an Actions task for a push to a protected branch (→ `trusted`) or for PR `n` (→ `pr-<n>`). Developer PATs are read-only on `trusted` and write to `dev-<user>`.
  - Stage 1 (no fork): writes to `trusted` require a token in `turbo.trusted_tokens` (a secret configured only on the trunk-deploy environment). Others write to `pr-<n>`, with `n` taken from the `x-asc-pr` header, which is untrusted and only isolates PRs from each other. Known limitation: same-repo PR workflows can read repository secrets, so the trusted token must live in a protected Forgejo environment.
- Retention: `pr-*` 14 days and `trusted` 60 days, by last access (a daily sweep).
- `OPTIONS` preflight returns presigned S3 URLs for artefacts > 8 MiB when the store is S3.
- Developers set `TURBO_API` to the same server, which gives one cache across all their worktrees and CI's trusted scope (read).

### 12.5 ASC changes independent of asc (do now)

Delete the comment-only `.gitattributes`. Replace `"env": ["*"]` on `build` with an explicit list. Add `check-types` to every package. Add `cargo-check`/`cargo-test` turbo tasks for `apps/analysis-pipeline` (inputs `**/*.rs`, `**/Cargo.toml`, `Cargo.lock`). Bind-mount the bun cache on runners. Merge `ci-intelligence.yml` into `ci.yml`. Use `UV_LINK_MODE=hardlink`. Add `.asc/projects.toml` (U64 proposes one): tag the docker services `image` and the Coolify targets `deploy`, and list the CI workflow files under `[global] inputs` if desired. `deploy-affected.ts` may read `asc affected --json` instead of `turbo ls --affected`.

### 12.6 Post-merge and pre-deploy testing (no merge queue) [audit: 28-agent-conflict-api]

- There is no global lock. Every land and merge emits Forgejo's push event, so post-merge CI runs on land-branch tips (Actions `concurrency` per ref, `cancel-in-progress: false`). When CI falls behind it tests the newest tip, and skipped tips count as covered by it.
- When all required jobs pass, the post-merge workflow moves `refs/heads/green/<branch>` to the tested tip with `UpdateRefs(FAST_FORWARD)`. Deploy workflows build only from `green/*`.
- On failure, automation or an agent bisects with the API alone: `Log(first_parent, green/<b>..<b>)`, CI results per tip, then `Apply(REVERT, update_ref=<b>)` for the first failing landed changeset, or a fix forward. `FindLanding` (§8.9) maps the changeset to its PR.
- **Stack queue (§20.5 flow 5):** the Forgejo "merge stack when ready" queue is ordered merge-when-ready over `UpdateRefs(LAND)`: one land in flight per target branch, the largest ready prefix landed atomically, no speculative CI, no batching and no global lock. Post-merge testing as above still covers every land. This does not contradict "no merge queue".

### 12.7 ASC Actions fork: CI uses asc, not git (user decision)

- **One action set [audit: actions-repo-naming, actions-fork-contract-and-naming]:** repository `asc/actions` on Forgejo holds the forked `checkout`, `cache`, `upload-artifact` and `download-artifact`, plus `asc/setup` (installs asc), `asc/changed` (`asc ci changed`), `asc/affected` (`asc ci affected`) and `asc/turbo-cache` (exports `TURBO_API/TOKEN/TEAM/CACHE` as §12.3 step 5 for jobs without checkout). The forks keep upstream's inputs and outputs (`ref`, `fetch-depth`, `path`, `submodules`, `lfs`, `sparse-checkout`), mapped to asc semantics: `fetch-depth` is ignored (no `shallow`; history beyond the window comes from the server, §12.3 step 4), `submodules` uses composition (§10.6), `lfs` is accepted and ignored (large files are native, §2.5), `sparse-checkout` maps to cones.
  - `checkout` calls `asc ci checkout` (plain, sparse or virtual) and falls back to `actions/checkout`'s git path only for repos that are not asc (discovery, §6.2).
  - **Cache and artifact store [audit: actions-cache-artifact-underspecified]:** `cache` and the artifact actions upload file content with `HasObjects`/`PutObjects` into the repository's namespace as ordinary `File`/`Chunk` objects (FileIds; chunked over 4 MiB), so they dedupe with repository content. Each entry is a manifest `ActionsManifest { version: u8 = 1, entries: Vec<(RepoPath, Kind, FileId, u64)> }` (postcard, §2.2a), written with `PUT /actions/v1/<owner>/<repo>/<cache|artifact>/<key>` and read with `GET` on the same path. The server derives the scope from the token as §12.4 does (task token; PRs cannot write `trusted`; reads try the caller's scope, then `trusted`) and stores the manifest at `actions/<owner>.<repo>/<cache|artifact>/<scope>/<pct(key)>` (§3.3), where `pct` percent-encodes every byte except `A–Z a–z 0–9 . _ -`, so a key prefix is a listing prefix. `GET /actions/v1/<owner>/<repo>/cache?prefix=<restore-key>` returns the newest matching manifest, as `actions/cache` restore keys do. Retention as §12.4. Every unexpired manifest is a GC root for its FileIds (§3.6).
  - Artifacts stored in asc are listed and downloaded by `asc ci artifact list|download` and the forked `download-artifact` (also across runs). They are not shown on Forgejo's run page in v1 (§19 decision 5).
- **Transparent adoption:** the Forgejo fork sets `[actions] DEFAULT_ACTIONS_URL` to the mirror, and the runner fork has an alias table that maps `actions/checkout@*` to `asc/actions/checkout` (likewise `cache`, `upload-artifact`, `download-artifact`). Existing workflows switch with no YAML edits.
- **Runner-native fast path:** the runner fork recognises the checkout action and runs `asc ci checkout` on the host (sharing the host cache, §12.3) instead of running a node action in the job container. It runs `asc ci checkout` with `ASC_HOME=<job dir>/asc-home` and bind-mounts the tree, that `ASC_HOME`, and the `asc` binary directory into the container at identical absolute paths, so the gitfile's absolute target resolves inside the job. It then sets `GITHUB_ENV`/`GITHUB_PATH` as in §12.3 step 6.

---

## 12A. Polyglot project graph and affected (crate `asc-graph`) [audit: projgraph-section-and-model]

asc finds projects and their dependencies itself for four ecosystems: Rust (Cargo), Python (uv, pyproject, requirements), .NET (sln, slnx, csproj, fsproj, vbproj) and Node (package.json workspaces, Bun). It never runs cargo, uv, dotnet, bun, turbo or nx. Detection is a pure parse of manifest and lockfile bytes read from the object store. The graph is therefore derived data per changeset: it needs no checkout and no hydration, and it runs the same way in local-only mode, on a bucket remote and on the server. Turbo and nx stay optional consumers of the output (§12A.9).

### 12A.1 Model and ownership (`asc-graph::{model,own}`)

`asc-graph` is pure: no I/O and no async. It depends only on `asc-core`, plus `toml`, `serde_json`, `quick-xml` and `globset` (exact pins in U01). JSONC files (`bun.lock`, `turbo.json`) first go through the in-crate `jsonc.rs` pre-pass, which strips `//` and `/* */` comments and trailing commas. Derivation, hashing and all I/O live in `asc-engine::projects`.

```rust
pub enum Ecosystem { Rust = 0, Python = 1, Dotnet = 2, Node = 3, Declared = 4 }   // postcard order frozen
pub enum ProjectKind { Package = 0, Workspace = 1, Solution = 2 }
pub struct ProjectKey { pub eco: Ecosystem, pub manifest: RepoPath }            // identity; unique
pub struct Project {
    pub key: ProjectKey,
    pub name: Vec<u8>,          // crate name | PEP 503-normalised dist name | project file stem | package.json name | solution file name
    pub kind: ProjectKind,
    pub root: Option<RepoPath>, // manifest directory; None = repository root
    pub deps: Vec<(ProjectKey, DepKind)>,   // sorted, unique
    pub extra_inputs: Vec<Vec<u8>>,         // repo-relative globs the manifest references outside root
    pub config_fp: [u8; 32],    // shared configuration that applies to this project (§12A.4)
    pub external_fp: [u8; 32],  // resolved third-party closure (§12A.4)
    pub tags: Vec<Vec<u8>>,     // from .asc/projects.toml
    pub gitlink: Option<RepoPath>, // Some(G) when the project lives inside gitlink G (§12A.2.6)
    pub opaque: bool,           // manifest unparsable, over a limit, or uses an unsupported construct
}
pub enum DepKind { Normal = 0, Dev = 1, Build = 2, Peer = 3, Optional = 4, Implicit = 5, Contains = 6, Member = 7 }
pub struct ProjectGraph { pub projects: Vec<Project> /* sorted by key */, pub config: ResolvedConfig, pub issues: Vec<Issue> }
pub struct Issue { pub path: RepoPath, pub message: Vec<u8> }
```

These types use the §2.2 encoding rules; their envelope kinds are listed in §12A.5.

- **Display id:** `<eco>:<name>`, with the ecosystem spelled `rust`, `python`, `dotnet`, `node` or `declared`. Input also accepts the aliases `cargo` → rust, `uv` → python, `csharp`/`fsharp` → dotnet, `bun`/`js` → node. When two projects of one ecosystem share a name, both display as `<eco>:<name>@<root>` (ASC has two unrelated Cargo workspaces).
- **Selectors** (CLI, RPC and config): a display id, a bare unique name, `path:<root>`, `tag:<t>` or `eco:<eco>`. Any selector may be prefixed by `!` to exclude.
- **Kinds:**
  - a `Package` is a buildable unit: a crate, a Python distribution, a .NET project file, a package.json with `name`, or a declared project;
  - a `Workspace` is a Cargo virtual workspace (`[workspace]` without `[package]`) or a uv workspace root without `[project]`;
  - a `Solution` is a `.sln` or `.slnx`;
  - Workspaces and Solutions have `Member` edges to their members, so they are affected exactly when a member is. CI uses them to run `cargo test --workspace` or `dotnet test X.sln` once.
- **Claimed paths:** every path a detector claims (manifests, lockfiles and ecosystem-global files; table in §12A.2), plus `.asc/projects.toml` and `.asc/ci.toml`. Claimed paths act only through fingerprints and global rules (§12A.4, §12A.7), never through ownership, except that a project owns its own manifest.
- **Ownership:** `owners(path)` = the `Package` projects whose root is the deepest ancestor directory of `path` among all Package roots. On a tie, all of them own it; for example `apps/analysis-pipeline/pipeline-api` belongs to both `rust:pipeline-api` and `node:analysis-pipeline-api`. Workspace and Solution projects own only their manifest. Paths matched by `ignore` (§12A.3) and paths with a `node_modules` component have no owner. A non-claimed path with no owner is **unowned** (§12A.7 step 3e).
- **Contains edges:** a Node `Package` P whose root is not the repository root gets `(Q, Contains)` for every non-Node `Package` Q whose root equals P's root or lies below it. Node packages are what bun and turbo scripts run, and they wrap native code. ASC: `node:@asc/dsl-compiler` contains the eight `rust:dsl-*` crates; `node:analysis-pipeline-api` contains `rust:pipeline-api`; `node:@asc/soma` contains `python:asc-soma`. Edges between other ecosystems come only from manifests. `.asc/projects.toml` can remove any edge.
- **Opaque projects:** a project is opaque when its manifest fails to parse, exceeds a §12A.10 limit, or uses an unsupported construct (§12A.2). It keeps its root and ownership, gets `(Q, Implicit)` to every other project of its ecosystem, and is listed in `issues`. One bad manifest never fails the whole graph.

### 12A.2 Detectors (`asc-graph::detect::{rust,python,dotnet,node}`) [audit: detectors]

```rust
pub enum Claim { Manifest, Lockfile, EcoGlobal }
pub trait Detector: Send + Sync {
    fn ecosystem(&self) -> Ecosystem;
    fn claim(&self, path: &RepoPath) -> Option<Claim>;                      // basename/suffix only
    fn parse(&self, claim: Claim, bytes: &[u8]) -> Result<Parsed, Issue>;  // pure; memoised by FileId (§12A.5)
    fn link(&self, cx: &LinkCx<'_>) -> Vec<Project>;                       // all parsed files of this ecosystem -> projects
}
pub const DETECTORS: [&dyn Detector; 4] = [&rust::Rust, &python::Python, &dotnet::Dotnet, &node::Node];
```

"Pluggable" means one module that implements `Detector` plus one line in `DETECTORS`; there is no dynamic loading. `parse` never sees the path, so its results are shared by content. `link` resolves relative paths: `LinkCx` gives it the manifest set (paths and parsed values), the gitlink paths, and a lookup from a directory to the projects of every ecosystem. Glob expansion (workspace members) runs against the manifest set only: a glob matches directory D when D's manifest is in the set. No tree walk happens, and no file outside the manifest set is read.

| Eco | Manifests | Lockfiles | Eco-global (a change affects every project of the ecosystem under the file's directory) |
|---|---|---|---|
| rust | `Cargo.toml` | `Cargo.lock` | `rust-toolchain`, `rust-toolchain.toml`, `.cargo/config.toml`, `.cargo/config` |
| python | `pyproject.toml`, `requirements*.txt`, `setup.py`, `setup.cfg` | `uv.lock` | `.python-version`, `uv.toml` |
| dotnet | `*.csproj`, `*.fsproj`, `*.vbproj`, `*.sln`, `*.slnx`, `Directory.Packages.props` | `packages.lock.json` | `Directory.Build.props`, `Directory.Build.targets`, `global.json`, `nuget.config` (any case), `.config/dotnet-tools.json` |
| node | `package.json` | `bun.lock`; patch files named by root `patchedDependencies` | `bunfig.toml`, `.npmrc`, `.nvmrc`, `.node-version`, root `turbo.json`; unsupported lockfiles `bun.lockb`, `package-lock.json`, `pnpm-lock.yaml`, `yarn.lock` |

**12A.2.1 Rust (Cargo).**
- A `Cargo.toml` with `[package]` is a Package named by `package.name`. One with `[workspace]` and no `[package]` is a Workspace. A root with both is a Package that is also a member of its own Workspace.
- **Members** = the `[workspace].members` globs, relative to the workspace directory and matched against directories that hold a `Cargo.toml`, minus `exclude`. A crate belongs to the nearest ancestor workspace that lists it; `package.workspace` is honoured. A crate in no workspace is standalone (ASC: `nela-risk-wasm`, `soma-candle-runner`).
- **Dependency tables:** `[dependencies]` (Normal), `[dev-dependencies]` (Dev), `[build-dependencies]` (Build), and the same three under `[target.'<cfg>'.*]`. The crate name is the dependency key, or `package = "..."` when renamed.
- **Internal dependencies:** a dependency with `path` (resolved against the manifest directory; the edge goes to the Package rooted there); one with `workspace = true` whose `[workspace.dependencies]` entry has `path` (resolved against the workspace directory; ASC: `dsl-napi` → `dsl-core`, `dsl-ir`); or one that a `[patch.<source>]` entry in the workspace root gives a `path`. Every other dependency is external.
- **Extra inputs:** `path` values in `[lib]`, `[[bin]]`, `[[test]]`, `[[bench]]`, `[[example]]` and `package.build` that leave the crate root.

**12A.2.2 Python.**
- A `pyproject.toml` with `[project]` is a Package named by `project.name`, normalised per PEP 503 (lowercase; every run of `-`, `_`, `.` becomes `-`). One with `[tool.uv.workspace]` and no `[project]` is a Workspace; its members are the `members` globs minus `exclude`, matched against directories that hold a `pyproject.toml`.
- **Dependency tables:** `project.dependencies` (Normal), `project.optional-dependencies.*` (Optional), `[dependency-groups]` and `tool.uv.dev-dependencies` (Dev), `build-system.requires` (Build). The name is the PEP 508 leading identifier, with extras and markers dropped (`soma[onnx]` → `soma`).
- **Internal dependencies:** only when `[tool.uv.sources]` (the project's own table first, then the workspace root's) maps the name to `{ workspace = true }` (edge to the member with that name) or `{ path = "..." }` (edge to the Package at that directory; `editable` makes no difference). `git`, `url` and `index` sources are external (ASC: `asc-soma` → `soma` is a pinned git source).
- **`requirements*.txt`** belongs to the Python Package in the same directory, or else creates a Package named by the directory path. `-r <file>` and `-c <file>` add `extra_inputs`; `-e <path>`, a bare relative path and `<name> @ file:<path>` add Normal edges; every other line is external.
- **`setup.py`/`setup.cfg`** without a `pyproject.toml` `[project]` make an opaque Package named by `setup.cfg` `[metadata] name`, else the directory name.

**12A.2.3 .NET.**
- Each `*.csproj`, `*.fsproj` and `*.vbproj` is a Package named by the file stem, rooted at its directory. XML is read with `quick-xml`. `Condition` attributes are ignored, so every item counts (conservative).
- **`<ProjectReference Include="...">`:** one Include may hold several `;`-separated values. Each: backslashes become `/`; `$(MSBuildThisFileDirectory)` and `$(MSBuildProjectDirectory)` become the project directory; the result is resolved against the project directory (exact match first, then a unique ASCII case-insensitive match). Any other `$(...)` makes the project opaque.
- **Extra inputs:** `<Import Project="...">` values, and `Compile`/`Content`/`None`/`EmbeddedResource` `Include` values that leave the project directory (globs kept as globs).
- **`<PackageReference Include|Update="N" Version|VersionOverride="V">`** is external; its version comes from the item, else the nearest ancestor `Directory.Packages.props` `<PackageVersion Include="N" Version="V">`.
- **Solutions:** each `*.sln`/`*.slnx` is a Solution project named by its file name, with Member edges to the projects it lists: in `*.sln`, the `Project("{type}") = "<name>", "<path>", "{guid}"` lines whose path ends in a project extension (solution folders skipped); in `*.slnx`, every `<Project Path="..."/>` at any depth. Unlisted projects are still detected.
- **`Directory.Build.props`/`.targets`** are eco-global; every one in an ancestor directory applies, not only the nearest (conservative).

**12A.2.4 Node (package.json, Bun).**
- **Workspace root:** the shallowest `package.json` with `workspaces` (array, or `{ "packages": [...] }`). Member globs match directories that hold a `package.json`. A `!` pattern removes the directory it names, or the directory whose `package.json` it names (ASC: `!packages/soma/sdk/package.json`). The root is a Package when it has `name` (ASC: `node:@asc/private`, turbo's `//`). A `package.json` inside a workspace that the globs do not match is not a project (as in bun and turbo). A `package.json` with `name` outside any workspace is a standalone Package.
- **Dependency tables:** `dependencies` (Normal), `devDependencies` (Dev), `peerDependencies` (Peer), `optionalDependencies` (Optional).
- **Internal dependencies:** a `workspace:<anything>` spec (edge to the member with that name); a `file:`/`link:` relative path (edge to the Package there); or any other spec whose name equals a workspace member's name (ASC: `"@asc/dsl-compiler-linux-x64-gnu": "*"` resolves to the `npm/*` members). `npm:<other>@<range>` aliases are external under `<other>`. `catalog:` specs are external; the lockfile records how they resolve (§12A.4).
- A nested `turbo.json` is owned by its package like any other file.

**12A.2.5 Declared projects.** A `[[project]]` entry in `.asc/projects.toml` with `name` and `root` and no `select` creates a `declared:<name>` Package. It owns paths like any Package; its only edges are its `implicit_dependencies`.

**12A.2.6 Gitlinks [audit: gitlink-workspace-members].** A detector cannot read inside a gitlink.
- **Node members inside a gitlink:** when a Node workspace glob reaches under gitlink G, the members under G are taken from the root `bun.lock` `workspaces` entries whose key is under G; their name and dependency tables come from the lock entry (ASC: `@soma/budgets`, `@soma/inference`, `@soma/oracle` under `packages/soma/sdk`).
- These projects have `gitlink = Some(G)` and own nothing. They are affected when the gitlink entry at G changes (§12A.7 step 3f); Normal edges reach them, so `node:@asc/soma` and its dependents follow.
- Without a `bun.lock` the members are unknown, and an issue is reported.
- Other ecosystems inside gitlinks (ASC's `soma/sdk/control` Cargo workspace) are not seen. Declare a project with `root = G` to make a pin bump trigger its dependents.

### 12A.3 `.asc/projects.toml` (optional; read from the revision being analysed) [audit: projects-toml-overrides]

```toml
version = 1
ignore = ["**/fixtures/**", "examples/**"]   # never define projects, never owned, never trigger anything

[global]
inputs = [".forgejo/workflows/ci.yml"]   # a change to any match makes every project affected
unowned = "ignore"                        # "ignore" (default) | "all": what a changed path with no owner does
builtin_eco_globals = true                # use the §12A.2 eco-global defaults

[ecosystem.node]
inputs = [".env.example"]                 # extra eco-global inputs for this ecosystem
[ecosystem.dotnet]
enabled = false                           # turn a detector off

[named_inputs]                            # built-in: default = ["{projectRoot}/**"]
production = ["default", "!{projectRoot}/**/*.md", "!{projectRoot}/**/*.test.ts", "!{projectRoot}/tests/**"]

[targets.build]                           # built-in for any target: ["default", "^default"]
inputs = ["production", "^production"]
[targets.test]
inputs = ["default", "^production"]

[[project]]
select = "node:@asc/plugin-runtime"     # must match exactly one detected project
inputs = { test = ["default", "^production", "{workspaceRoot}/schemas/**"] }
implicit_dependencies = ["rust:dsl-napi", "!node:@asc/ui"]   # add edges (kind Implicit) / remove detected edges
tags = ["image", "deploy"]

[[project]]                               # declared project (§12A.2.5)
name = "migrations"
root = "packages/database/migrations"
tags = ["db"]
```

- **Pattern grammar:** globset globs over repository paths. `{projectRoot}` expands to the project root; `{projectRoot}/**` matches only paths the project owns (§12A.1) plus its `extra_inputs`, so nested projects never leak in. `{workspaceRoot}` is the repository root and matches any path. A bare identifier is a named input. `^name` means the same named input of each dependency (§12A.6, §12A.7 step 4). `!` excludes; exclusions apply after all inclusions.
- **Validation:** an unknown key, a cycle between named inputs, a `select` that does not match exactly one project, or a declared `root` that does not exist in the tree is an error that names the line. `asc affected` then exits 1, and the server returns `PROJECTS_CONFIG` (§12A.8).
- **Which revision's file:** each graph uses its own revision's file (base graph: base's file; head graph: head's file). A change to the file itself makes everything affected (§12A.7 step 3a).

### 12A.4 Configuration and lockfile fingerprints [audit: lockfile-aware-fingerprints]

Each Package carries two 32-byte fingerprints, computed at link time as `blake3::derive_key("asc v1 project fp", canon(...))`. `canon` (`asc-graph::canon`, golden-tested) is the canonical encoding of parsed TOML, JSON and XML values: maps sorted by key, encoded with the §2.2 postcard rules. A fingerprint changes only when something changes that can change this project's build. §12A.7 compares base and head fingerprints, so a shared file never forces "everything changed".

| Eco | `config_fp` covers | `external_fp` covers |
|---|---|---|
| rust | From the workspace root: `[workspace.package]`, `[profile.*]`, `[patch.*]`, `[replace]`, `resolver`, `[workspace.lints]`, and the `[workspace.dependencies]` entries this crate inherits | From the governing `Cargo.lock` (the workspace directory's, else the crate's): every package reachable from this crate through `dependencies`, stopping at local packages (no `source`), as sorted unique `(name, version, source, checksum)`. With no `Cargo.lock` tracked (ASC today): the crate's external dependency declarations (name, version requirement, features) |
| python | The workspace root's `[tool.uv]` table, `requires-python`, and the `[tool.uv.sources]` entries the project uses | From the governing `uv.lock`: packages reachable from this member through `dependencies`, `optional-dependencies` and `dev-dependencies`, stopping at workspace, editable, virtual and directory sources, as `(name, version, source, sdist.hash)`. With no lock: the declared external requirements |
| dotnet | The non-`PackageVersion` content of every ancestor `Directory.Packages.props`, plus the `PackageVersion` entries for the names this project references | The `FileId` of `packages.lock.json` in the project directory, if present; otherwise the sorted `(name, resolved version)` of its `PackageReference`s |
| node | The workspace root `package.json` fields `packageManager`, `engines` and `trustedDependencies` | From `bun.lock`, the closure below. With no `bun.lock`: the declared external specs |

**The bun.lock closure:** (1) start at the member's entry in the `workspaces` map; (2) resolve each dependency name `d` requested by key `k`: try `k/d`, then each shorter parent key followed by `/d` (`a/b/@s/n` → `a/b` → `a`, a scoped name counting as one segment), then `d` alone; (3) from each resolved `packages` entry follow its `dependencies`, `optionalDependencies` and `peerDependencies`, stopping at `workspace:` entries; (4) collect each entry's `("name@version", integrity)`, plus the patch file's `FileId` when root `patchedDependencies` names that `name@version`.

Effects: a `bun.lock` diff that only bumps `react` affects only the members whose closure contains `react`, plus their dependents; a `Cargo.lock` bump of `tokio` affects only the crates that reach `tokio`; editing `[profile.release]` in `packages/dsl/core/rust/Cargo.toml` affects only that workspace's crates; one `PackageVersion` bump affects only the .NET projects that reference it. Parse results are keyed by content (§12A.5), so the 1.1 MiB `bun.lock` is parsed once per distinct content.

### 12A.5 Derived data (incremental, no checkout) [audit: derived-data-incremental]

| Name | Key | Value | Computed from |
|---|---|---|---|
| `manifests.v1` (`impl Derived`, §5) | changeset | `{ set: [u8; 32] }` | the parent's set, patched with this changeset's `changes` |
| `mset.v1` (envelope kind 20) | set id = `blake3::derive_key("asc v1 manifest set", encoding)` | sorted `Vec<(RepoPath, Kind, [u8; 32])>`: every claimed path (§12A.2) with its `FileId`, and every gitlink (its 20-byte oid, zero-padded) | – |
| `parsed.v1` (kind 21) | blake3(eco ‖ claim ‖ FileId) | the detector's parsed form | one file's bytes |
| `pgraph.v1` (kind 22) | set id | `ProjectGraph` | the parsed files of the set, including `.asc/projects.toml` |
| `phash.v1` (kind 23) | blake3(set id ‖ root TreeId ‖ target) | `Vec<(ProjectKey, [u8; 32])>` | §12A.6 |

- **Deriving `manifests.v1`:** if no `changes` key is claimed (by basename) and none is a `Change::Gitlink`, the value is the parent's set id: O(1), which covers most ASC commits. Otherwise copy the parent's set, apply each claimed change in turn (`Write`/`Gitlink` inserts or replaces, `Delete` removes) and store the new set. A root changeset walks its tree once. Merges use `parents[0]` (§2.6).
- **Deriving `pgraph.v1`** does not recurse over parents; it is a function of the set alone. It fetches `parsed.v1` for every entry and parses (and stores) only `FileId`s never parsed before, which in practice are the manifests a changeset touched. Linking all detectors and applying the config takes O(number of manifests) in memory (ASC: about 140 files).
- **Reads:** only manifest and lockfile bytes are ever read, through `Objects` (machine cache, bucket or server store), never through a working copy. A virtual workspace therefore hydrates nothing, and the server needs no checkout.
- **Server warming:** the background task `asc-serve::projects_warm` follows `ref_log` and derives `manifests.v1` and `pgraph.v1` for every new tip of `refs/heads/*` and `refs/pull/*/head`; land-branch tips are warmed first (§5). It is not on the `UpdateRefs` path. A cold request derives synchronously (bounded by one link per distinct set). The server also keeps an in-memory LRU of graphs keyed by set id.
- **Storage:** these values are recomputable, so they live under `d/` and `cache/d/` (evictable), never in `store/`.

### 12A.6 Per-project input hashes (no hydration) [audit: input-hashes-without-hydration]

`input_hash(P, T, cs)` is a cache key for target `T` of project `P` at changeset `cs`. It reads trees only (`Entry.id` is the content hash) and never reads file content.

```
input_hash = blake3::derive_key("asc v1 project input",
    postcard((P.key, T, files_digest, P.config_fp, P.external_fp, eco_globals_digest, deps)))
```

- **`files_digest`:** let S be P's inputs for `T` (§12A.3) without the `^` items. Walk `cs.root`, and for each directory D compute `decide(S, D)`: `All` when some include pattern covers `D/**` and no exclude pattern, nested project root or `ignore` pattern can match under D (decided from literal prefixes; a pattern starting with `**` can match anywhere); `None` when no include pattern's literal prefix overlaps D; `Partial` otherwise. An `All` directory contributes `(D, Dir, TreeId)`, `None` contributes nothing, and `Partial` recurses, each selected file contributing `(path, kind, FileId)`. The digest is blake3 over the sorted contributions, so an unchanged subtree costs one tree id.
- **`eco_globals_digest`:** the sorted `(path, FileId)` of the eco-global files that apply to P, plus the files matching `[global] inputs`.
- **`deps`:** for each `^name` item, the sorted `(dep key, dep's input hash for that named input)` over P's dependencies (all DepKinds). Dependency cycles (Cargo dev-dependency cycles, Node peer cycles) collapse into strongly connected components: inside a component `^` uses the members' own `files_digest`, outside it the full hashes.
- **Memoisation:** `phash.v1`.
- **Scope:** these are asc's own hashes (CI cache keys, a future `asc run`). Turbo and nx still compute their own task hashes inside jobs (§12.1).

### 12A.7 Affected (normative) [audit: affected-algorithm]

Inputs: a base `B`, a head `H`, an optional target `T`, an optional ecosystem filter, optional selectors.

1. **Changed paths.** `M = merge_base(B, H)`, or `M = B` with `--no-merge-base`. `C` = the changed paths of `leafdiff(M.root, H.root)`, hash-pruned (on the server one `Diff(NAMES)`). Both sides of a rename are in `C`. With `--uncommitted`, also add the working copy's staged, unstaged and untracked-but-not-ignored paths.
2. **Graphs.** `Gb = pgraph(M)`, `Gh = pgraph(H)`. With `--uncommitted`, `Gh` parses dirty claimed files from the working copy and stores nothing.
3. **Direct set `D`** (each entry records its reason):
   a. `C` touches `[global] inputs`, `.asc/projects.toml` or `.asc/ci.toml` → every project, `all = true`, reason `global:<path>`; go to step 5.
   b. An eco-global file in `C` (§12A.2 table or `[ecosystem.<e>] inputs`) affects every project of that ecosystem whose root is under the file's directory. Reason `eco-global:<path>`.
   c. Projects in `Gh` whose key is not in `Gb`. Reason `new`.
   d. Projects in both graphs where `config_fp` differs (reason `config`), `external_fp` differs (reason `lockfile`), or the `FileId` of its own manifest, its `deps` or its `extra_inputs` differ (reason `manifest`).
   e. For each non-claimed path `c` in `C`, `owners(c)` in `Gh` (and in `Gb` for paths deleted in H). An owner is added only if `c` matches its inputs for `T` (without `T`: the `default` named input); reason `file:<path>`. A project whose `extra_inputs` match `c` is also added. A path with no owner is recorded in `unowned`; with `[global] unowned = "all"` it acts as in 3a.
   f. A `Change::Gitlink` at G adds the projects with `gitlink == Some(G)` (reason `gitlink:<G>`) and the owners of G as in 3e.
4. **Closure.** Add every project that reaches a member of `D` along `Gh` edges, reason `dep:<id>`. All DepKinds count, including Contains, Member and opaque projects' Implicit edges. With `--target T`, an edge from P to Q is followed only if T's inputs for P contain a `^` item; the built-in `^default` is one, so with the default config `--target` does not narrow the closure.
5. **Removed, filters, order.** `removed` = keys in `Gb` not in `Gh`; reported, never counted as affected. `--type` and `--select` filter the result only (the closure has already crossed ecosystems). Order is topological over `Gh`: dependencies first, cycles as components, ties by display id.

Result: `Affected { base, head, merge_base, all, projects: [{ project, direct, reasons, input_hash? }], removed, unowned, issues, changed_paths }`. An empty result is success. Detectors MUST report every edge they can see; an edge they cannot see (for example a TypeScript path alias with no package.json dependency) is fixed with `implicit_dependencies`. The only deliberately conservative parts are opaque projects and Contains edges.

### 12A.8 RPCs (`proto/asc/v1/projects.proto`) [audit: server-rpcs]

```proto
syntax = "proto3";
package asc.v1;
option go_package = "forgejo.org/modules/ascproto/ascv1";
import "asc/v1/repo.proto";

service Projects {
  rpc ProjectGraph(ProjectGraphRequest) returns (ProjectGraphResponse);
  rpc Affected(AffectedRequest) returns (AffectedResponse);
  rpc ProjectHashes(ProjectHashesRequest) returns (ProjectHashesResponse);
}
enum Ecosystem { ECOSYSTEM_UNSPECIFIED = 0; ECOSYSTEM_RUST = 1; ECOSYSTEM_PYTHON = 2; ECOSYSTEM_DOTNET = 3;
                 ECOSYSTEM_NODE = 4; ECOSYSTEM_DECLARED = 5; }
enum ProjectKind { PROJECT_KIND_PACKAGE = 0; PROJECT_KIND_WORKSPACE = 1; PROJECT_KIND_SOLUTION = 2; }
message ProjectDep { string id = 1; string kind = 2; }   // normal|dev|build|peer|optional|implicit|contains|member
message ProjectInfo { string id = 1; Ecosystem ecosystem = 2; bytes name = 3; bytes root = 4; bytes manifest = 5;
  ProjectKind kind = 6; repeated ProjectDep deps = 7; repeated string tags = 8; bytes gitlink = 9; bool opaque = 10; }
message DetectIssue { bytes path = 1; string message = 2; }
message ProjectGraphRequest { RepoRef repo = 1; string rev = 2; repeated Ecosystem types = 3; repeated string select = 4; }
message ProjectGraphResponse { bytes changeset = 1; bytes graph_id = 2; repeated ProjectInfo projects = 3;
  repeated DetectIssue issues = 4; }
message AffectedRequest { RepoRef repo = 1; string base = 2; string head = 3; bool no_merge_base = 4;
  repeated Ecosystem types = 5; string target = 6; repeated string select = 7; bool with_hashes = 8; }
message AffectedProject { ProjectInfo project = 1; bool direct = 2; repeated string reasons = 3; bytes input_hash = 4; }
message AffectedResponse { bytes base = 1; bytes head = 2; bytes merge_base = 3; bool all = 4;
  repeated AffectedProject projects = 5; repeated string removed = 6; repeated bytes unowned = 7;
  repeated DetectIssue issues = 8; uint32 changed_paths = 9; }
message ProjectHashesRequest { RepoRef repo = 1; string rev = 2; string target = 3; repeated string select = 4; }
message ProjectHashesResponse { bytes graph_id = 1; map<string, bytes> by_project = 2; }
```

**IDE target commands (§24.8.2, A4) [audit: ide-plugins-folded]:** a `[[project]]` may carry `argv = { test = ["bun", "run", "test"] }` in `.asc/projects.toml`. `AffectedProject` gains `map<string, TargetCmd> targets = 5` with `message TargetCmd { repeated string argv = 1; }` (made by P57). asc stores and serves it and never runs it; an IDE "Run target" task runs it with the project root as cwd.

- **Access:** read permission on the repo (§15.1). **Ids:** with `id_mode = GIT`, changeset fields are git oids. **Order:** `projects` in §12A.7 order. **Feature flag:** `VersionResponse.features` includes `projects.v1`.
- **Bad config:** `INVALID_ARGUMENT` with `ErrorDetail.code = PROJECTS_CONFIG` and the file's path.
- **Latency targets on ASC:** `Affected` < 300 ms warm and < 2 s cold (a new manifest set, including parsing the 1.1 MiB `bun.lock`). The plan-job target in §1 stays < 10 s.
- **Forgejo:** from Stage 3, Forgejo may call `Affected` to show an "affected projects" panel on a PR. CI needs no Forgejo change.

### 12A.9 CLI and output formats [audit: cli-and-output-formats]

```
asc affected [--base <rev>] [--head <rev>] [--no-merge-base] [--uncommitted]
            [--type rust|python|dotnet|node ...] [--target <name>] [--select <selector> ...]
            [--format list|json|turbo|nx|matrix|gha] [--matrix-by project|workspace|ecosystem]
            [--json] [--explain] [--hashes] [--repo <owner/repo>]
asc projects [<selector> ...] [--rev <rev>] [--type ...] [--format list|json] [--owner <path> ...]
asc graph    [--rev <rev>] [--type ...] [--format json|dot] [--hashes [--target <name>]]
```

- **Defaults [audit: affected-default-base-contradiction]:** `--head` = `HEAD`. `--base` = `refs/remotes/origin/HEAD`, else `refs/remotes/origin/<default branch>`, else the local default branch (`master`, §6.5); the comparison point is `merge_base(base, head)` unless `--no-merge-base` (§12A.7 step 1). A pushed feature branch therefore compares against the default branch, not its own upstream. `--json` = `--format json`. `--type` accepts the §12A.1 aliases, including `csharp`.
- **Exit codes:** 0 on success, including an empty result; 1 for an invalid `.asc/projects.toml` or a selector that matches nothing; 128 for a revision that does not resolve.
- **Where it runs** (§11.2 notation): **L** in local-only and bucket-remote repos (the local engine reads manifests through `Tiered`, fetching missing manifest blobs, never hydrating the working copy); **S** when the repo has a server remote and both revisions are pushed (`asc.serverHistory=auto`); **S** always with `--repo` (no workspace needed; discovery and credentials as for `asc ci`). `--uncommitted` always runs locally.
- `asc projects --owner <path>` prints the owning projects of each path, or `claimed by <eco>`, or `unowned`. `asc graph --format dot` prints Graphviz.
- **Formats** (projects by display id; machine formats never use colour):
  - `list` (default): one display id per line, in §12A.7 order; with `--explain`, a tab and the reasons;
  - `json`: `{"version":1,"base","head","mergeBase","all","projects":[{"id","ecosystem","name","root","kind","direct","reasons","hash"}],"removed","unowned","issues"}`; changeset ids are git SHA-1s (§11.1);
  - `turbo`: `--filter=<name>` for each affected Node Package except the repository-root package, space-separated on one line. Non-Node projects reach this list through Contains edges; affected non-Node projects that no Node package contains are printed on stderr as `asc: not visible to turbo: <ids>`. Empty output means nothing is affected, but turbo treats no filter as everything, so callers MUST gate on `count`;
  - `nx`: comma-separated names for `nx run-many --projects=`, same rules as `turbo`;
  - `matrix`: `{"include":[{"project","name","ecosystem","root","workspace"}]}`, where `workspace` is the root of the project's Workspace or Solution, else its own root. `--matrix-by workspace` emits one row per Workspace, Solution or standalone Package; `--matrix-by ecosystem` one row per ecosystem;
  - `gha`: lines for `$GITHUB_OUTPUT`: `affected=<JSON array of ids>`, `count=<n>`, `all=<bool>`, `rust=<bool>`, `python=<bool>`, `dotnet=<bool>`, `node=<bool>`, `turbo_filter=<turbo>`, `nx_projects=<nx>`, `matrix=<matrix JSON>`. It works on Forgejo Actions unchanged.
- These commands have no git equivalent (§11.2), so they never change git behaviour. With the shim, `git affected` dispatches by the §11.4 order, so a user alias or `git-affected` wins.

### 12A.10 Limits and trust [audit: limits-and-trust]

- **No execution:** detectors never run anything (no `cargo metadata`, `uv`, `msbuild`, `bun`, `turbo`). An unsupported dynamic construct makes a project opaque. Detectors never guess.
- **Limits:** manifests ≤ 4 MiB (always inline objects, §2.4); lockfiles ≤ 64 MiB; nesting depth ≤ 128; ≤ 200,000 lockfile packages; ≤ 100,000 projects per graph. A manifest over a limit gives an `Issue` and an opaque project. A lockfile over a limit gives every governed project `external_fp` = the lockfile's `FileId`, so any change to it affects them all.
- **Config tampering:** any change to `.asc/projects.toml` or `.asc/ci.toml` makes everything affected (§12A.7 step 3a), so a PR cannot narrow its own CI by editing the config.
- **Manifest changes:** narrowing CI through manifests (dropping a dependency) is a reviewed code change, as with turbo and nx.
- **Backstop:** post-merge jobs on land branches recompute affected against trunk (§12.6): Scion's post-merge testing instead of a merge-queue lock.

---

## 13. Forgejo: full storage replacement

### 13.1 Stages

| Stage | Source of truth | Forgejo reads | Forgejo writes | Fork needed | Rollback |
|---|---|---|---|---|---|
| 0 git-remote | Forgejo git | git | git | no | uninstall asc |
| 1 mirror | Forgejo git | git | git | no (reverse proxy for `/.well-known/asc`) | stop asc serve |
| 2 asc truth | **asc** | git **bridge** (eager) | temp-repo pushes into the bridge, imported by pre-receive | patches 1–9 (K01–K09) | `storage_backend=git` (the bridge is current) |
| 3 Seam A | asc | **asc** (`ascbackend`, id_mode GIT) | as Stage 2 | + K10, K11 | `storage_backend=dual`/`git` |
| 4 Seam B | asc | asc | **asc** (`Commit`/`Merge`/`Apply`/`UpdateRefs`) | + K12, K13 | as above while the bridge is eager |
| 5 Actions | asc | asc | asc | + K14 | – |
| 6 IDs (optional, later) | asc | asc, `object_format=asc` | asc | + K16 | migration down script |

At Stage 4 no bare git repository is a source of truth. The bridge becomes `lazy`: it is built on the first legacy git request, for `git clone`, `actions/checkout`, push mirrors or external submodule consumers.

### 13.2 Two independent switches [graft: integration D2]

- `repository.storage_backend`: `git` | `dual` | `asc`. This is a new column (K08). Selection happens in one place, `modules/gitrepo.OpenRepository`. `dual` serves from `gitcli` and calls `ascbackend` on a 10% sample of read calls (configurable), comparing results and emitting `forgejo_asc_shadow_mismatch_total{method}` plus a log line. It never changes a response.
- `repository.object_format_name`: `sha1` (unchanged) or `asc` (Stage 6). In `sha1` mode, `ascbackend` sends `id_mode = GIT`, and `asc serve` translates ids at its boundary through `git_map`. No database rows change at cutover. Commit statuses, PR merge bases, comments, Actions runs and links keep working.

### 13.3 Patch stack (Go, `D:\dev\forgejo`, branch `asc/v16`, one commit per unit)

| Unit | Patch | Behaviour change |
|---|---|---|
| K01 | Fork baseline and CI (`make test-backend`, sqlite and pg integration) | none |
| K02 | Path hiding: 34 `git.OpenRepository(path)` callers → `gitrepo.OpenRepository`; `repoPath string` signatures (blame, diff, log-name-status) → `*Repository` | none (upstreamable) |
| K03 | Seam A: `Backend` interface (about 30 methods, as in `forgejo.md` §2) inside `git.Repository`; current code moves to `modules/git/gitcli` | none (upstreamable) |
| K04 | Seam B: `modules/repostore.Writer` (`CreateCommit`, `Merge` incl. dry run, `CherryPick`, `Revert`, `ApplyPatch`, `UpdateRefs`, `Init`, `Fork`, `Delete`, `Size`); temp-repo code moves to its `gitcli` implementation | none |
| K05 | Remove raw `git.NewCommand` outside `modules/git/gitcli` and `services/ascbridge`; add the `build/check-raw-git` linter | none (upstreamable) |
| K06 | `modules/ascproto` (buf-generated from `source-control/proto`, committed) plus `modules/ascclient` (connection, auth, retry, LRU by immutable id) | none |
| K07 | `routers/private/asc.go`: `POST /api/internal/asc/authz` → `{allowed, user_id, username, perm, deploy_key_id, is_actions_task, task: {run_id, event, ref, pr_index, is_fork_pr}, branch_rules: {require_signed: bool}}`; serve `/.well-known/asc` from `[asc] SERVER_URL` | behind `[asc] ENABLED` |
| K08 | `storage_backend` column, migration, `[asc]` settings, backend selection in `gitrepo.OpenRepository` (gitcli for `git`/`dual`) | behind the flag |
| K09 | Stage 2 hook glue: in `HookPreReceive`, for `storage_backend=asc` and git-origin pushes, after policy checks call `ImportGitPush(quarantine paths, updates)`; an asc error rejects the push. In `HookPostReceive`, for `storage_backend=asc`, process only `PushTrigger="asc"` calls (asc's outbox is the single fan-out) and return OK for git-origin calls. Internal writers of `refs/pull/*` (AGit proc-receive handling, PR head sync) call asc `UpdateRefs(refs/pull/<n>/head, SKIP_PRE)` first and write the bridge only through `bridge_promote` (§13.5) | behind the flag |
| K10 | `modules/git/ascbackend`: the `Backend` over `asc.v1.Repo` (`ReadChangeset`, `ReadTree`, `ReadFile`, `Stat`, `ListRefs`, `Log`, `MergeBase`, `IsAncestor`, `LastCommits`, `Blame`, `Diff`, `Archive`, `Grep`, `CheckAttr`, `Resolve`); `services/gitdiff` parses `Diff(PATCH)` text unchanged; language stats computed in Go over `ReadTree` plus `ReadFile` | `storage_backend=asc` |
| K11 | `modules/git/dual` shadow backend | `storage_backend=dual` |
| K12 | `modules/repostore/asc`: one RPC per Writer method (`Commit`, `Merge`, `Apply`, `UpdateRefs`, `RepoAdmin`); `CopyClosure` for cross-repo PRs; `refs/pull/<n>/head` via `UpdateRefs` after `CopyClosure` | `storage_backend=asc` |
| K13 | `services/ascbridge`: `bridgePath(repo)` used by `githttp.go`, `cmd/serv.go`, `services/mirror`, migrations; `bridge=off\|lazy\|eager`; lazy build = `asc export --git <bridge>`; no LFS serving: for `storage_backend=asc` repos the LFS endpoints are disabled (historic LFS content is archived outside asc, §14.3); repo lifecycle (fork O(namespace copy), delete, size = logical) | `storage_backend=asc` |
| K14 | Actions: `paths:` filters via `Diff(NAMES)`; context gains `asc_server_url`, `asc_changeset`, `asc_base_merge_base`; `github.sha` stays the git SHA in `sha1` mode | `storage_backend=asc` |
| K15 | Integration suite matrix: `git`, `dual` (zero mismatches required) and `asc` | test only |
| K16 | ID mode `asc`: `object_format_name=asc`, a one-off migration rewriting stored SHAs through `GitMap`; every input accepts both forms | per repo, optional |
| K17 | Relaxed approvals by change class: branch protection `asc_relaxed_classes`/`asc_relaxed_approvals`, per-file class badge from `Classify` (§8.10) | per branch rule |
| C34 | Stacked PRs: PR ↔ change (`asc_change`), `stack_parent_id`, the `merge_base` rule of §20.5 and its recomputation patch, AGit lookup by change, update authorisation | `storage_backend=asc` |
| C35 | `SetReviewState` notifier on PR, review and check events | as C34 |
| C36 | Stack panel, needs-restack banner, mid-stack merge block (web and `POST /pulls/{n}/merge` → 409 `stack-parent-unmerged`) | as C34 |
| C37 | Versions dropdown, interdiff, "since my last review" | as C34 |
| C38 | "Merge stack (N)", post-merge `RestackChanges`, approvals kept when `patch_unchanged` | as C34 |
| C39 | Stack queue: merge-when-ready cascade, order, pause, revalidate; the `RequestMerge` internal endpoint (§12.6) | as C34 |
| C40 | Actions stack context (`asc_change`, `asc_stack_position`, `asc_stack_size`, `asc_stack_parent_sha`; K14 extension) | as C34 |
| C41a–c | Launchpad-style inbox: state model (whose turn, pin, snooze), `GET /api/v1/user/asc/inbox`, page | as C34 |
| C42 | Commit graph page over `CommitGraph` | `storage_backend=asc` |
| C43 | Overlap and mergeability badges; stack compare view | as C34 |
| C44 | Suggested changesets (v1.x) | as C34 |
| C46 | Branch-chain PR retarget (§20.5 flow 6) | as C34 |
| C47 | "Restore version" (moves `refs/pull/<n>/head` to a kept version) | as C34 |
| C48 | "Revert landed stack" (`Apply(REVERT)` over `FindLanding`, newest first) | as C34 |
| B17 | Branchless repo settings (branchless, cloud, `shared_refs`), no branch rows or push events for user refs, draft PRs by default, draft-PR `pull_request` trigger if spike S13 requires it (§21.5.2) | `storage_backend=asc`, per repo |
| B23 | "Your drafts" page over `Cloud.GetWorkspace` (v1.x; labels escaped) | as B17 |
| M20 | MCP framework [audit: mcp-folded]: `/api/mcp`, stateless Streamable HTTP 2026-07-28, `server/discover`, reuse of API auth, Origin check, `[mcp]` settings, read-only gate (write methods also need `lockdown`, M28), PRM document. SDK: the official `modelcontextprotocol/go-sdk` pinned to its first 2026-07-28 release, else a thin own POST transport with only the SDK's types | `[mcp] ENABLED = true`, `WRITE = false` |
| M21 | `pr_read` (methods `get` with `AscHandles`, `list`, `search`, `reviews`, `review_comments`, `comments`, `checks`, `stack`); vendors `asc_handles.schema.json` | as M20 |
| M22 | `pr_write` (stack-aware merge, 409 `stack-parent-unmerged` returned as `isError`, auto-merge) and `review_write` (confirmation on approve and dismiss) | writes behind `WRITE` |
| M23 | `issue_read`, `issue_write` (confirmation on every method but `label`) | as M22 |
| M24 | `actions_read` (ranged and tailed logs, 64 KiB cap) and `actions_write` | as M22 |
| M25 | `inbox_read`, `inbox_write` | as M20 |
| M26 | OAuth: RFC 8707 `resource`, `aud` in access tokens, RFC 9207 `iss`, the pre-registered public client `asc-mcp`, `aud` and scopes in the K07 authz answer | OAuth provider additions |
| M27 | (v1.x) CIMD client registration | as M26 |
| M28 | `lockdown` mode: hide content by authors without write access in `pr_read`, `issue_read` and `actions_read`; registers `lockdown.Available()` | as M20 |
| I23 | Code-indexer backend `asc` [audit: codeintel-folded]: `[indexer] REPO_INDEXER_TYPE = asc`; `modules/indexer/code/asc`; `Index` and `Delete` are no-ops, `Search` calls `CodeIntel.Search` at the default branch | `storage_backend=asc` |
| I24 | Repo search page: mode tabs (exact, regex, symbols, semantic), scope selector, coverage note; shows server-redacted text only | as I23 |
| I25 | File-view symbol panel and outline; PR "find usages in the base" | as I23 |
| I26 | `/explore/code` fan-out (RRF across readable repos) and `/api/v1/repos/{owner}/{repo}/asc/search\|symbols\|semantic` (also the hook for the Forgejo MCP endpoint) | as I23 |
| P04 | Read-only asc RPC proxy `/-/asc/rpc/<service>/<method>` [audit: ide-plugins-folded]: gRPC-Web in, raw passthrough to `asc serve` with the internal token, a method allowlist (read RPCs only), repo read permission checked first (§24.5.3) | `storage_backend=asc` |
| P05 | Actions REST for plugins: runs and jobs by `asc_change`, ranged and tailed job logs (reuses the M24 services) | as P04 |
| P06 | OAuth public client `asc-ide` (PKCE; `vscode://`, `cursor://`, `vscodium://` redirects and loopback redirects on any port; RFC 8252 §7.3); separate from `asc-mcp` (M26) | OAuth provider additions |
| P07 | `asc-ui` islands: the vendored bundle at `public/assets/asc-ui/<version>/`, the mount partial `templates/base/asc_ui.tmpl`, a version pin | as P04 |
| P58 | Inbox count: `GET /api/v1/user/asc/inbox?section=&limit=` returns `X-Total-Count` and honours `If-None-Match` (a hunk in C41b) | as C41b |

The ASC Actions fork (§12.7) needs no Forgejo patch: `[actions] DEFAULT_ACTIONS_URL` is an upstream setting. The runner fork (`forgejo-runner`, Go, separate repo) carries the alias table and the native checkout fast path (units R01–R02).

**Patch-stack upkeep [fix: missing-from-all]:** budget 2–4 engineer-days per upstream minor release (most conflicts are in `modules/git`, where K03 moved code verbatim to `gitcli`; `build/rebase-map.sh` lists upstream changes to `modules/git/X.go` against `gitcli/X.go`). K02, K03 and K05 are offered upstream as a "storage backend interface"; each one accepted shrinks the stack. The fork moves from v16 to v17 once K01–K05 are stable.

### 13.4 Forgejo lifecycle mapping

| Forgejo operation | Stage ≥ 4 behaviour |
|---|---|
| Create repo | `RepoAdmin.init` (new namespace) |
| Fork | `RepoAdmin.fork` (namespace copy job; the UI shows "preparing" until done) |
| Transfer/rename | `RepoAdmin.rename` |
| Delete | `RepoAdmin.delete` |
| Size/quota | `RepoAdmin.size` → logical size used for quota |
| Wiki | a second asc repo `<owner>/<name>.wiki` |
| Mirrors (pull) | git fetch into the bridge, `ImportGitPush`-equivalent import, `UpdateRefs` |
| Mirrors (push) | bridge forced `eager`, git push from the bridge |
| AGit `refs/for/*` | §13.5: never stored; proc-receive returns `refs/pull/<n>/head`, which asc updates |
| Archive download | `Archive` |
| Code indexer | `Diff(NAMES)` between indexed and new tip, plus `ReadFile` |
| Repo doctor | `RepoAdmin.verify` |

### 13.5 AGit and PR head refs [audit: agit-refs-pull-stage2]

- `refs/for/*` is never stored as a ref (§2.7).
- **Stage ≥ 4:** `asc push origin HEAD:refs/for/<b>[/<topic>] -o topic= -o title= -o description= -o force-push`. `UpdateRefs` ingests the closure, then calls `POST /api/internal/hook/proc-receive/{owner}/{repo}` with the same `HookOptions`. Forgejo returns the PR ref (`refs/pull/<n>/head`) and `is_new`/`is_force`. asc performs that ref update through the normal pipeline (`HOOK_MODE_SKIP_PRE`, internal token) and answers the client with `RefResult{name = refs/pull/<n>/head}` plus Forgejo's message (the PR URL), which `asc push` prints as git does.
- **Stacked PRs (§20.5):** `asc submit` pushes `refs/for/<trunk>/asc-<change>`, so the AGit topic is `asc-<change>`. The fork (C34) finds the PR by `(repo, change)`, not by `(topic, user)`, and applies the update authorisation of §20.5 flow 1.
- **Stages 2–3:** K09 also patches Forgejo's internal writers of `refs/pull/*` (proc-receive handling and PR head sync, which call `gitrepo.UpdateRef` or push to the base repo). For `storage_backend=asc` they call asc `UpdateRefs(refs/pull/<n>/head, SKIP_PRE)` first and write the bridge only through `bridge_promote`. Existing `refs/pull/*` move across at the Stage 2 flip through the final mirror import (§8.6 already fetches `+refs/pull/*`).

---

## 14. Migration and import (crate `asc-git`)

### 14.1 Import

`asc import --git <path|url> [--repo <name>] [--incremental]` (also used by git-remote mode, mirror mode and `ImportGitPush`; it obeys §14.4):

1. Use `gix` to list commits not yet in the git map, in topological order (parents first).
2. For each commit: diff its git tree against its first parent's tree (gix tree diff, no rename detection), giving the changed leaves. Ingest new blobs (BLAKE3, SHA-1 kept, chunking over 4 MiB). LFS pointer blobs are ordinary blobs (§2.5). Gitlinks become `Change::Gitlink`.
3. Build asc trees bottom-up from the git trees. Each `Entry.git` is the actual git oid. Any git tree whose canonical render differs from its raw bytes gets an override `o/<oid>`.
4. Build the changeset with import canonicalisation (§2.9) and validate it (§2.6).
5. Write `gen`, `gitcommit` and the git map rows. **Assert `gitcommit(changeset) == original commit oid`**, and abort on mismatch.
6. Tags: annotated tags become `TagRecord`s, canonical or raw. A tag whose target is not a commit (a tree, a blob, or another tag), lightweight or annotated, cannot be represented. The importer skips it, records it in `ImportReport.skipped_refs` (name, oid, target type), and prints `warning: asc: skipped tag '<name>' (points to a <type>)`. `verify-import`, `asc_roundtrip` and mirror ref sync treat skipped refs as expected, not as mismatches **[audit: import-non-commit-tags]**.

LFS content is never imported (§2.5).

ASC scale: 5,502 commits and about 50k blobs (6.99 GiB raw) import in minutes. The 4.6 GB unreachable cruft pack is never read.

### 14.2 Export

`asc export --git <dir> [--refs <glob>]` and the bridge writer render blobs (file bytes), trees (override or render) and commits (raw or canonical) for every changeset not yet in the target. They write loose objects, or one undeltified pack when there are more than 1,000 objects, then index it with `git index-pack`. Refs are set with `git update-ref --stdin`. Both run through `RealGit` (§11.4). Export is deterministic, so the same changeset always yields the same SHA.

### 14.3 ASC cutover runbook (unit O03)

1. **Prep (no asc):** §12.5 hygiene. Archive the 25.6 GiB LFS store once to a cold S3 bucket, outside asc (No LFS, §2.5). Optionally `git gc --prune=now` on the server mirror copy to drop cruft (not required).
2. **Stage 0 (week ≈ 6):** pilot users install asc (MSI). `asc clone https://forge.example.org/asc/dds D:\work\dds --reference D:\dev\monorepo\.git`. Agents use `asc worktree add --ephemeral`. `asc doctor --report-reclaimable` lists the old `.git` (58 GB), the 49 git worktrees and the submodule git dirs. **The user deletes them; asc never does.**
3. **Stage 1 (week ≈ 10):** deploy `asc serve` (Postgres schema `asc` on Forgejo's DB server; Garage bucket `asc`, or local disk). `RepoAdmin.init mode=mirror` for `asc/soma` and `asc/dds`. Add the webhook and the proxy route for `/.well-known/asc`. Run `asc serve admin verify-import` (every commit's export equals git, which is required). Client workspaces switch automatically by discovery: ids are identical, so nothing is re-fetched. CI moves to the aliased `actions/checkout` (§12.7) and the plan job.
4. **Stage 2 (week ≈ 14):** deploy the fork with K01–K09. Freeze pushes for minutes, run a final mirror import, and flip `repos.mode=primary`, `forgejo_reads=git`, `bridge=eager`, `storage_backend=asc`. The existing bare repo is now the bridge. **Rollback drill on staging first:** set `storage_backend=git`, `repos.mode=mirror` (once the outbox has drained). The bridge is current and SHAs are identical.
5. **Stage 3 (week ≈ 18):** `storage_backend=dual` for a week (zero mismatches above noise), then `asc` with `forgejo_reads=asc`.
6. **Stage 4 (week ≈ 22):** K12–K13; bridge → `lazy`. Full storage replacement.
7. Later: Stage 6 IDs (optional); fold soma (user decision); artefacts into asc (user decision).

### 14.4 Ingestion invariants (normative for every importer and every server ingest path) [audit: 22-ingest-invariants]

- **I1 Closure before reference:** an object is written only after every object it references is durable in the same namespace (files and chunks → trees bottom-up → changesets → tags).
- **I2 Parents first:** changesets are ingested in topological order. A `cs_graph`/`cs_gen` row is written only after the rows of all its parents exist.
- **I3 Commit point:** a changeset's kind-3 `git_map` row is written last, after the changeset object, its eager derived values and its `cs_graph` row. Incremental import selects commits that have no kind-3 row, so a crash at any step re-imports that commit and produces identical bytes.
- **I4 Refs last:** a ref (asc ref, mirror sync, `ImportGitPush`) moves only when I1–I3 hold for its whole new closure (the warm-ref rule, §5).
- **I5 Idempotent writes:** object puts are create-if-absent, and map and graph inserts are `INSERT … ON CONFLICT DO NOTHING`. A second concurrent importer of the same commits gets no error and writes no different row.
- **I6 One importer per repo:** mirror import, the poller and `ImportGitPush` for one repo are serialised by `pg_advisory_xact_lock(repo_id)` (SQLite has a single writer).
- **I7 Bounded memory:** commits stream in topological order through a bounded window. Peak RSS is O(window + largest tree + chunk buffer), independent of history length. Blob hashing (BLAKE3, SHA-1) runs in parallel across files; changeset construction stays sequential.

**Targets:** ASC full import ≤ 5 min with peak RSS ≤ 1 GiB on 8 cores; a synthetic 1M-commit history imports in < 6 h with peak RSS ≤ 2 GiB. **Test `import_crash`:** 200 seeded runs each kill the importer at a random object write and rerun it; the final store, git map and `cs_graph` must be byte-identical to an uninterrupted import.

---

## 15. Security

### 15.1 Identity and authorisation

- **Identity is Forgejo's.** Clients present a Forgejo PAT, OAuth token or Actions task token (env `ASC_TOKEN` when set, as in CI jobs, §12.3 step 6; else `git credential fill` with `host = <forge host>`; `asc auth login` stores a PAT via `git credential approve`).
- `asc serve` authorises every repo-scoped call. Stage 1 uses Forgejo's public `GET /api/v1/repos/{owner}/{repo}` with the token, checking `permissions.pull`/`push`. Stage 2+ uses the internal authz endpoint (K07). Positive answers are cached for 60 s and negative ones for 5 s, keyed by `(sha256(token), repo, mode)`.
- **Object access is per namespace** (one per repo, §3.3). Content ids are 256-bit, but the namespace rule, not unguessability, is the control.
- `HOOK_MODE_SKIP_PRE` and `ImportGitPush` require Forgejo's internal token, over the private network or mTLS.
- **CI:** a job's token is bound to its repo; reading a composed repo needs a composition grant (§15.5). With the host cache read-only and the job cache per job, cross-job poisoning is impossible. Every consuming tier verifies BLAKE3 (§3.2).
- **Daemon:** a current-user-only pipe/socket. The daemon never serves another user.
- **MCP [audit: mcp-folded]:** `asc serve` is an OAuth 2.1 resource server for `/mcp` (§22.7.3). A request without a token gets 401 with `WWW-Authenticate: Bearer resource_metadata=…, scope="asc:read"`. The scopes are `asc:read` and `asc:write`; too little scope gets 403 `insufficient_scope`. Forgejo is the authorisation server (M26), and asc checks that the token's `aud` equals its canonical `/mcp` URL through the K07 authz answer. Tokens are never passed through to another API. A Forgejo PAT is accepted as the bearer for headless agents and CI (`read:repository` maps to `asc:read`, `write:repository` to `asc:write`); it has no audience, because Forgejo issues it for its own resource set. Every call authorises `(principal, repo, read|write)` through the cache above, and an unreadable repo returns the same `NOT_FOUND` as a missing one. `asc mcp` over stdio uses the CLI credentials and no OAuth.

### 15.2 Integrity and policy

- All object reads are verified (§3.2). The server re-derives every client-supplied git field (§8.4) and validates every changeset (§2.6).
- Path safety: `checkout_safe` on every materialisation (git's NTFS/HFS protections); `windows_safe` on new changesets (§2.8).
- Policy stays in Forgejo's pre-receive: branch protection, protected files, signed-commit rules and quota run on the exact changesets that will be referenced (rewritten ones for land).
- Signing: git-format signatures over canonical commit bytes (§2.9), verified by Forgejo and git unchanged. Turbo artefacts carry HMAC tags.
- **Drafts and secrets [audit: branchless-folded]:** drafts upload in the background only in a branchless, non-public repo whose server has `cloud` on (§21.3.3), and only after the pre-upload guard has held ignored and denied paths and changesets with a detected secret (§21.3.7); `push` and `submit` apply the same guard unless `--no-secret-check`. Uploaded draft objects are readable by id by every repo reader (the namespace rule, §15.1), while listing another user's workspace needs repo write (§21.2.5). An owner can purge their own unsubmitted drafts and a repo admin any; purge also queues `cache_evict` to every cache tier, and copies already fetched to client machines cannot be recalled. A per-user quota applies to cloud ingest and user-ref pushes. `asc.cloud=off` and `asc.branchless=false` disable upload.
- Untrusted manifest and lockfile content from PR branches is parsed under the limits of §12A.10.
- Supply chain: pinned crate versions, `cargo deny` (licences: MIT/Apache/BSD/ISC/Zlib/Unicode only; no GPL; Sapling, Mononoke and EdenFS are ideas only), `cargo audit` in CI, release binaries signed (Authenticode on Windows, minisign for `asc upgrade`).
- **Code intelligence [audit: codeintel-folded]:** `usearch` (C++ FFI, cargo feature `ann`, off in `asc.exe`) is on the unsafe/FFI allow-list for `asc-codeintel::vectors` only, and `onig` (C, through fastembed's tokenizer) for `asc-embed`; both get an MSVC CI job. Model files are pinned by sha256 and minisign-verified (§16.2), model licences are checked against an allow-list (MIT, Apache-2.0), and indexes, secret handling and prompt-injection controls are in §23.7.
- **Unsafe code [audit: unsafe-forbid-and-allowlist]:** the workspace lint is `unsafe_code = "deny"`. Only the modules listed in PLAN "Rules for implementers" may opt in with `#![allow(unsafe_code)]`, and each wraps its calls in a safe API. Prefer safe crates and tools: `same-file` for file identity, and `schtasks.exe /Create /XML` for the logon task.

### 15.3 Data at rest and backup/DR [fix: missing-from-all]

- Bucket server-side encryption. Logs carry no file content.
- **Backup:** Postgres PITR (WAL archiving, 14 days) plus bucket replication. Because objects are written before refs, are never deleted on the write path, and GC roots include all ref-log targets within the PITR window (§3.6), restoring Postgres to time T always yields refs whose closure exists in the bucket. Bucket versioning is not required. Off-site copies MUST NOT propagate deletes for less than the PITR window **[audit: gc-sweep-race-and-pitr-roots]**.
- **Restore drill** (quarterly, `docs/runbooks/restore.md`): restore Postgres to T on staging, point an `asc serve` at the bucket, run `asc serve admin verify --all` (every ref's closure exists; git map round-trips), then rebuild the bridge with `asc export`. The drill passes when verify exits 0.

### 15.4 Trust boundaries (normative; prevents over-engineering) [audit: 25-trust-boundaries]

- **Untrusted, validated once at entry:** client-supplied objects and git fields (§8.4), tokens, webhook payloads (HMAC), git objects read from quarantines and git-remote repos, files on disk in workspaces, `.asc/*.toml` content, manifests and lockfiles (§12A.10).
- **Trusted, never re-validated:** the server's own SQL rows; derived values the server wrote; Forgejo internal API responses over the internal token; `asc serve` ↔ Forgejo calls on the private network; the daemon ↔ CLI pipe for the same user; the server's own bucket.
- **BLAKE3 verification where an object is consumed (§3.2)** is the only integrity check on object reads. No other layer re-hashes or re-parses an object it got from `Objects::get`.
- **Review rule:** a review finding that asks for more validation of a trusted component must name a concrete failure scenario with inputs. Without one, it is closed as "trusted per §15.4".

### 15.5 Directory-level permissions [audit: 26-directory-permissions]

- **Write:** path rules are Forgejo protected-file patterns plus path-scoped required approvals (K17). Pre-receive evaluates them on the changeset `changes` keys, which is metadata only (`Diff(NAMES)` from Stage 3). Violations return `PATH_POLICY` with the paths.
- **Read:** asc does not filter objects by path inside a repository. A content-addressed object can appear under any path, so per-path checks on `GetObjects` would be unsound and costly; this is dropped deliberately. A sensitive directory is instead its own repository, composed at a gitlink (§10.6). The namespace rule (§15.1) then enforces read access for every RPC. When a user has no read access to the nested repo, the workspace projects the gitlink path as an empty read-only directory, and `asc status` prints `<path>: restricted (no access to <repo>)`.
- **CI and composition:** a task token is bound to its repo, so reading a composed repo needs an explicit grant: `asc-serve.toml [[composition_grants]] parent = "asc/dds", child = "asc/soma"`. The grant gives the parent's Actions task tokens read-only access to the child for the token's lifetime. Without a grant, `asc ci checkout` fails with `PATH_POLICY: <path> needs read on <child>`.
- **Code intelligence [audit: codeintel-folded]:** the read rule holds for derived data too. Each namespace has its own indexes, the server authorises read on a namespace before it opens that namespace's indexes, and a composed child is searched only when the caller can read it (§23.7). A future path-level deny rule would subtract the denied subtree in the same predicate.

---

## 16. Operations

### 16.1 Observability (per tier, agent-readable) [audit: 29-observability-superfans]

- **Logs:** JSON lines via `tracing` with stable fields `ts, level, tier (cli|daemon|warmer|server|cache|forgejo), event, repo, op_id, rpc, duration_ms, outcome, trace_id`. `event` names are a stable catalogue (`land.conflict`, `land.retry`, `outbox.retry`, `outbox.dead`, `cache.corrupt`, `switch.pending`, `reconcile.orphan`, `import.mismatch`, `rate.limited`, `hydration.denied`, `daemon.restart`, `daemon.hung`). Daemon logs rotate at 50 MiB × 5. OTLP export when `ASC_OTLP_ENDPOINT` is set (any tier); `traceparent` is propagated from Forgejo through gRPC.
- **Metrics:** Prometheus. Server and cache role: `/metrics`. The daemon serves `--metrics-listen 127.0.0.1:<port>` (off by default; on for CI runner and agent-fleet hosts). The warmer writes its counters to the node-exporter textfile directory. The catalogue (names fixed, in `docs/metrics.md`):
  - **server/cache:** `asc_rpc_duration_seconds{rpc,code}`, `asc_store_op_seconds{op,backend}`, `asc_ref_cas_conflicts_total`, `asc_land_total{outcome}`, `asc_hook_seconds{hook,outcome}`, `asc_outbox_lag_seconds`, `asc_outbox_failures_total`, `asc_mirror_import_lag_seconds`, `asc_derive_lag_changesets`, `asc_changeset_reads{rpc}` (histogram, §5.1), `asc_graph_walk_nodes`, `asc_cache_requests_total{tier,result}`, `asc_rate_limited_total{class}`, `asc_inflight_requests`, `asc_shed_total{reason}`, `asc_coalesced_total`, `asc_turbo_hit_ratio{scope}`, `asc_rss_bytes`;
  - **daemon:** `asc_daemon_hydrations_total{process}`, `asc_daemon_hydration_seconds`, `asc_daemon_fetch_bytes_total`, `asc_daemon_cache_bytes{dir}`, `asc_daemon_journal_entries`, `asc_daemon_pending_paths`, `asc_daemon_reconcile_paths_total`, `asc_daemon_provider_errors_total{op}`, `asc_daemon_uptime_seconds`, `asc_rss_bytes`;
  - **code intelligence [audit: codeintel-folded]:** `asc_ci_index_lag_seconds`, `asc_ci_jobs{kind,state}`, `asc_ci_query_seconds{rpc}`, `asc_ci_candidates_ratio`, `asc_ci_coverage` (I29, runbook `docs/runbooks/tier-codeintel.md`);
  - **client:** `asc_client_retries_total`;
  - **warmer:** `asc_warm_lag_seconds` (trunk tip age in the host cache), `asc_warm_objects_total`, `asc_warm_failures_total`;
  - **forgejo:** `forgejo_asc_rpc_seconds{method}`, `forgejo_asc_shadow_mismatch_total{method}`.
- **Health:** `/healthz` (liveness) and `/readyz` (store, DB and Forgejo reachable; outbox lag < 5 min) on the server; `asc daemon status --json` for the daemon.
- **Runbooks:** `docs/runbooks/tier-<name>.md` per tier: normal ranges for each metric, the alert rules (`deploy/alerts.yml`), and the first diagnostic commands. They are written for both humans and monitoring agents.
- **Client diagnostics:** `asc debug stats [--json]` (cache size and hit ratio, bytes per workspace, hydrations per workspace, journal size, daemon uptime and RSS); `asc debug hydrations [--since] [--by-process]`; `ASC_TRACE=1` (per-command RPC timings); `asc doctor`.

### 16.2 Distribution, upgrades and version skew [fix: missing-from-all]

- Release CI builds `x86_64-pc-windows-msvc`, `x86_64/aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`.
- **Windows:** an MSI (`cargo-wix`) installs `asc.exe` and its private shim dir, optionally puts the `git` shim on the machine `PATH` before Git for Windows (§11.4, with the `~/.bashrc` block for Git Bash), adds `PATH`, enables `Client-ProjFS`, adds Defender exclusions for `$ASC_HOME`, and registers the per-user logon task. A winget manifest is published on ASC's Forgejo releases. Fleet rollout is through winget or GPO.
- **Linux:** a tarball, `install.sh` (installs the systemd user unit and optionally the shim), and a container image `asc:<ver>` for CI.
- **Side-by-side install [audit: upgrade-mechanics-windows]:** binaries install to versioned directories `<prefix>\<ver>\`, and `<prefix>\current` is a directory junction (symlink on Unix) that PATH points to. `asc upgrade` and the MSI write the new version side by side, recreate the shim link in the new directory, switch the junction, and then ask the running daemon to `Shutdown`. The next CLI call starts the new daemon (§9.1), and the old directory is removed at the next start. `asc upgrade` verifies the minisign signature first.
- **Update check:** the daemon checks the releases URL once a day, and `asc status` prints `hint: asc <v> is available (asc upgrade)`. `asc.autoUpgrade=true` makes the daemon run the upgrade itself while idle.
- **Skew policy:** the server's `Version.min_client` is enforced (`CLIENT_TOO_OLD`, with an upgrade hint). Servers support the current and previous minor clients. Envelope versions only change with a major release. Non-envelope encodings are versioned (§2.2a). Server schema changes are expand/contract (§8.2); `state.db` migrations run only after the daemon of another version is stopped (§9.2). The Forgejo fork checks `Version.features` at startup and refuses to enable `[asc]` when a required feature is missing.

### 16.3 Admission control, retries and failure isolation [audit: 18-overload-isolation, 24-rate-limiting]

- **Server rate limits:** a per-principal token bucket (keyed by the authz user id, or `sha256(token)` before authz) per RPC class:
  - `cheap` (`Version`, `ListRefs`, `Resolve`, `GitMap`, `HasObjects`, `Stat`, `MergeBase`, `IsAncestor`, `ReadChangeset`, `FindLanding`): 200 req/s, burst 400;
  - `objects` (`GetObjects`, `PutObjects`): 400 MiB/s;
  - `expensive` (`Log` without an index path, `Diff` PATCH, `Blame`, `Grep`, `Archive`, `Classify`, `Merge`, `Apply`, `Rebase`, `Commit`, `CopyClosure`, `Affected` cold): 20 concurrent per principal and 64 per replica;
  - `UpdateRefs`: 20 req/s per repo;
  - stacks (§20.5): `ResolveChanges`, `Stack`, `Versions`, `Overlaps` and `RepoConfig` are `cheap`; `InterDiff`, `Mergeability` (on a miss), `RestackChanges`, `RequestMerge`, `CommitGraph` with stats, `LineHistory` and pickaxe `Log` are `expensive`;
  - commit cloud (§21.3.5): `Cloud.GetWorkspace` and `ListWorkspaces` are `cheap`; `Cloud.UpdateWorkspace` and the uploader's `Ingest` run in a new class `background` that is shed first; `PutObjects` stays in `objects`;
  - MCP (§22.5) [audit: mcp-folded]: each call is charged to the class of its backing RPC (`log` is `cheap` or `expensive`, a `diff` with a patch is `expensive`), plus at most 8 in-flight MCP calls per principal per `asc serve` instance (a gateway enforces cluster limits on `Mcp-Param-Repo`). Over the limit, the call returns `isError` with `RATE_LIMITED` and `retry_after_ms`;
  - code intelligence (§23.4.1) [audit: codeintel-folded]: new class `codeintel`: `Search` and `Symbols` 50 req/s per principal (burst 100); `Semantic` 10 req/s per principal and 32 concurrent per replica (query embedding uses the CPU); `IndexStatus` is `cheap`; a search with no usable trigram counts as `expensive`. An MCP `search` call is charged to this class;
  - the `service` class (Forgejo internal token, CI warmer) has its own limits. All values live in `asc-serve.toml [limits]`.
- **Over limit:** gRPC `RESOURCE_EXHAUSTED` with `ErrorDetail.code = RATE_LIMITED` and trailer `retry-after-ms`; `/v8` and `/seeds` return 429 with `Retry-After`.
- **Load shedding:** each replica caps in-flight requests (`limits.max_inflight`, default 1,024) and 64 in-flight RPCs per token. Requests beyond a cap are shed at once (`UNAVAILABLE` or `RESOURCE_EXHAUSTED` with `retry-after-ms`) and never queued for longer than 1 s. Bucket calls have a per-replica concurrency cap (`bucket.max_concurrency = 256`) and a 30 s timeout.
- **Coalescing:** concurrent misses on the same `(ns, ObjKey)`, and on the same derived `(type, id)`, share one fetch or derivation (in-flight singleflight), in the primary and the cache role.
- **Client retries (CLI, daemon, warmer, Forgejo `ascclient`):** only `UNAVAILABLE`, `RESOURCE_EXHAUSTED` and `DEADLINE_EXCEEDED` are retried, with exponential backoff and full jitter (base 200 ms, cap 10 s, at most 5 attempts), honouring `retry-after-ms`. Each process has a retry budget: retries may be at most 10% of requests in a 10 s window; beyond that calls fail fast. A mutating call is never retried without its `x-asc-op-id`.
- **Strict tiering:** a client with an `objects` tier (§7) never falls back to `server` for objects: it tries the next `objects` URL, then fails with `fatal: asc: object tier unavailable`. CI jobs read only host cache → job cache → their configured tier. In server mode only `asc serve` (primary or cache role) talks to the bucket, apart from presigned URLs that the server issues. The warmer adds random jitter (§12.3).
- **Implementation:** `tower` `ConcurrencyLimit`, `LoadShed` and retry layers, a keyed `governor` limiter, and a generic `Coalesce<K>` singleflight in `asc-rpc`.

### 16.4 Resource budgets, cache SLOs and benchmarks [audit: 19-hit-rate-slo, user decisions efficiency and benchmarks]

- **Memory budgets (enforced by tests and O05):** the daemon's idle RSS is ≤ 64 MiB and stays bounded under load (tree cache ≤ 32 MiB, journal queue ≤ 8 MiB, per-provider buffers pooled). The server and cache role stream every response and never buffer a whole response or tree set; their caches are size-capped by config. Whole-file buffering happens only where an algorithm needs it (line diff, merge, blame, classify, grep), and is gated by a per-replica byte semaphore `limits.text_memory` (default 2 GiB): a request waits ≤ 1 s for budget, then is shed with `RESOURCE_EXHAUSTED` (§16.3). Files above `core.bigFileThreshold` (default 512 MiB) are treated as binary, as git does **[audit: server-whole-file-buffer-cap]**. The CLI never loads the full tree: it walks lazily, by spine.
- **`asc mcp` [audit: mcp-folded]:** idle RSS ≤ 24 MiB, and ≤ 64 MiB with 8 concurrent 25k-token diffs on the HUGE generator repo; at most 8 concurrent calls and a queue of 32 per process. The default `tools/list` estimates at most 6,000 tokens. Both are gated by M16 (§22.5).
- **Code intelligence [audit: codeintel-folded]:** the gates of §23.9 (exact search p50/p99 ≤ 50/300 ms on HUGE, ≥ 10x faster than `git grep <rev>` and cold `rg` on medium and HUGE, median-commit index ≤ 2 s without and ≤ 10 s with CPU embeddings, grams plus symbols ≤ 0.4x of indexed text, hybrid NDCG@10 ≥ BM25-only + 5 points) run in `bench/codeintel` (I27, I28) under the O05 gate. The daemon limit stays idle RSS ≤ 64 MiB and ≤ 96 MiB while serving CodeIntel.
- **ASC IDE plugins [audit: ide-plugins-folded]:** the budgets of §24.4.10, enforced by P44a (VS Code) and P44b (JetBrains) against checked-in baselines: `activate()` ≤ 30 ms and ≤ 5 ms in a non-asc folder (no bridge spawned); first SCM status ≤ 300 ms on ASC with a warm daemon; attributable extension-host heap ≤ 25 MiB at idle (measured by delta in a test-only window); bridge RSS ≤ 20 MiB; at most one webview at idle; graph first paint ≤ 150 ms and ≥ 55 fps over 100,000 rows with ≤ 5,000 rows in memory; host bundle ≤ 350 KB, `asc-ui` initial ≤ 300 KB gzip, JetBrains jar ≤ 4 MB. Never: `*` activation, a file watcher on `**`, `workspace.findFiles` over the repo, unpaged history, polling without a visible view.
- **Cache SLOs:** every tier exports `asc_cache_requests_total{tier=client|host|job|cache_role|server_mem|server_disk, result=hit|miss}`. Targets: object hit rate ≥ 99% at the CI host cache and at the cache role, and ≥ 95% at the server memory plus disk cache, measured daily. Three consecutive days below target raise an alert.
- **Every cache tier is disposable:** deleting the host cache, a cache role's disk, the server's `disk_dir` or a client's `cache/` loses no data and needs no repair. Only `store/`, `state.db`, server SQL and the bucket hold state.
- **Benchmark gate (O05, in CI):** a head-to-head harness (`bench/`, hyperfine-driven, plus a criterion suite for internals) runs the same scenarios on git and asc: clone (git full, `--filter=blob:none`, `--depth 1`, sparse-checkout and scalar where they apply, against asc virtual and plain), `worktree add`, `status` clean and dirty, `switch` between tips, `diff` (HEAD, merge-base, `--stat`), path-limited `log`, `blame`, `merge-base`/`is-ancestor`, `commit`/`push`/`fetch`, CI checkout plus affected, and turbo `--dry` hashing.
  - **Workloads:** small (monorepo, imported, about 4.5k files), medium (about 80k files and 1M commits, imported from a public repo such as the linux kernel), and HUGE (chromium-scale, ≥ 400k files and a 25+ GB working copy, from the deterministic synthetic generator `bench/gen` with configurable files, depth, size distribution, history length and churn, including 1M-file and 100 GB variants), plus 10x and 100x scaled variants of monorepo.
  - **Metrics:** wall time p50/p95, time to first usable shell, time to first build, bytes transferred, bytes on disk, files hydrated, peak RSS and server CPU, cold (empty machine cache) and warm (seed or shared cache).
  - **Gate:** asc must beat git on every scenario where Scion's model applies (clone, worktree, switch, CI checkout, merge-base, blame, log, disk and transfer) and be within 10% elsewhere (hot local `diff` and `status`). HUGE-repo virtual checkout completes in seconds, independent of repo size; `worktree add` cost does not depend on repo size. The HUGE `worktree add`/clone size-independence goal is measured with `asc.turboIndex=false`. With the turbo index on, the harness reports index render time separately, and the gate is "beats `git worktree add`" **[audit: synthetic-index-o-files]**. Medium and HUGE clone scenarios run with the default history window (§6.3). CI fails on a regression against the stored baseline.
  - **Results** are published to `docs/BENCHMARKS.md`: a table per release with the git version, machine spec and reproduction commands.

---

## 17. Testing strategy

| Layer | How |
|---|---|
| `asc-core` | proptest round trips (`decode(encode(x)) == x`, `encode(decode(b)) == b`); frozen golden vectors for ids and envelopes; git rendering against `git hash-object`/`mktree`/`commit-tree`/`mktag` outputs |
| `asc-store` | one conformance suite for every `Objects` and `Refs` impl: InMemory and LocalFileSystem always; MinIO and Garage (`--features it-s3`, docker compose); Postgres (`--features it-pg`); a concurrent CAS race (100 tasks, one winner per round); BucketRefs retry and ABA tests with a fault-injecting store; the put-fallback test with a store that rejects `If-None-Match` |
| `asc-engine` | tree apply/diff vs naive oracle; merge vs `git merge-file` and `git merge-tree --write-tree` goldens; diff output 100% byte-equal for Myers and histogram (§4.3), and every patch passes `git apply --check`; `merge_base` vs `git merge-base` on generated DAGs; skip-list and read-count bounds; land disjointness truth table (same path, case-fold, file-vs-dir prefix) |
| `asc-git` | import→export byte identity on fixture repos (signed commits, odd modes, LFS pointer files as plain blobs, gitlinks, encodings); native→export→import id equality; `import_crash` (§14.4); `asc_roundtrip` (`#[ignore]`, reads `D:\dev\monorepo` read-only, asserts all 5,502 SHAs) |
| `asc-wc`, `asc-vfs` | per OS: mount, enumerate, hydrate (counter), write→journal, switch with dirty/open files/tombstones, `reset --hard` with no HEAD change restores modified, Full, read-only and deleted (tombstoned) files, `restore <path>` of a deleted file, watcher sees switch-added files, directory rename and directory tombstones, daemon kill and reconcile, crash mid-switch (intent replay), case-insensitivity, unrepresentable names; ProjFS on a Windows runner with ProjFS enabled; FUSE in Linux CI or WSL |
| CLI | the git compatibility suite (§17.1); the shim (§11.4): byte-identical fall-through, no recursion, `--version` format, overhead budget |
| `asc-graph` | Per-detector fixture trees modelled on ASC: bun workspaces with a `!` negation and gitlinked members from bun.lock; two unrelated Cargo workspaces with a duplicate crate name plus standalone crates; `workspace = true` path deps and a `[patch]` path; a uv workspace with `workspace = true` and git sources; requirements files with `-e` and `-r`; a csproj with backslash `ProjectReference` paths and `$(MSBuildThisFileDirectory)`; central package versions, `.sln` with solution folders, nested `.slnx`. Lockfile tests: bumping one package affects exactly the members that reach it (bun.lock, Cargo.lock, uv.lock, Directory.Packages.props, packages.lock.json). Proptest: affected equals the naive full-recompute oracle. Golden encodings for kinds 20–23 |
| Project graph engine | only touched manifests are parsed (parse counter); a commit with no manifest change reuses the parent set; input hashes read 0 blobs outside the manifest set; a virtual workspace hydrates nothing during `asc affected` |
| Affected vs turbo | `asc_affected` (`#[ignore]`): for the last 300 ASC master commits, `asc affected --type node` must be a superset of `turbo ls --affected` (turbo 2.10.11 on a git clone) on every commit, and equal on at least 95% of them; differences are printed; cold and warm `Affected` latency are checked against §12A.8 |
| Turbo contract | `tests/turbo/`: pinned turbo 2.10.11, with the shim first on PATH; `--dry=json` hashes equal a git clone's; hydrations ⊆ manifests; one edit changes only its tasks; `turbo ls --affected` equals a git clone's; racy run has 0 verified entries; worktrees share `repos/<id>/.turbo/cache`; run on every turbo minor; a staged-but-uncommitted new file, a staged deletion and a staged modification each give the same `--dry=json` hashes as a git clone in the same state **[audit: turbo-staged-file-contract]** |
| Server | in-process `asc serve` over InMemory plus SQLite with a fake Forgejo (authz, hooks); every RPC; auth-denied paths; land races; outbox crash injection (kill between CAS and effects); Stage 2 quarantine ordering; GC sweep races; expand/contract N-1 compatibility; rate limits, shedding and coalescing |
| Simulation, fuzzing | §17.2, §17.3 |
| Benchmarks | §16.4 (O05 gate) |
| IDE plugins [audit: ide-plugins-folded] | the layers of §24.7.1: the Rust bridge tests (`bridge`, `bridge_run` with the argv injection corpus, `bridge_auth`), `bun test` for `@asc/asc-client` and the `asc-ui` logic, Playwright for `asc-ui` (axe, screenshots, 100,000-row graph trace), `@vscode/test-electron` on the bundled extension, the VSIX smoke test on VSCodium and VS Code, Gradle `HeavyPlatformTestCase`, IDE Starter + Driver (nightly) and `verifyPlugin`, Go tests for P04–P07 and P58, and the mock-Forgejo contract check (nightly) |
| Forgejo | upstream unit and integration suites on `git`, `dual` and `asc` (K15) |
| End to end | `deploy/compose.yml` (Garage, Postgres, asc serve, Forgejo fork, forgejo-runner): clone, push, PR, merge, plan job, cached turbo build, bridge `git clone`, legacy `git push`, rollback flip |

### 17.1 git compatibility suite (acceptance gate) [audit: compat-suite-gate]

- `tests/compat/` holds two kinds of case:
  - **Scenarios** `scenarios/*.t`: shell scripts that run `git` (resolved through PATH).
  - **Tool traces** `traces/<tool>/<case>.trace`: recorded invocations (argv list, env, stdin) captured by running the pinned tool against real git with a recording `git` wrapper (or `GIT_TRACE=1`), together with the scripted user action that drove it. Tools in scope: Claude Code, husky, lint-staged, commitlint, turbo, and ASC's repo scripts. IDE git integrations are out of scope (§0 decision 6).
- The runner runs each case twice on identical fixtures with a fixed environment (`HOME` with a fixed `.gitconfig` that sets `init.defaultBranch=master`; `GIT_AUTHOR_*`/`GIT_COMMITTER_*`; `TZ=UTC`; `LC_ALL=C`): first with real git, then with the asc shim installed as `git` first on PATH, in an asc workspace (plain mode, and virtual mode where available).
- **asc-side fixtures [audit: compat-fixture-routing]:** the asc side's `HOME/.gitconfig` also sets `asc.shim.init = asc`, so a scenario's `git init`/`git clone` create asc workspaces (§11.4). The shim counts fall-throughs to real git in `ASC_SHIM_TRACE`. The runner fails a case if the asc side fell through to real git at any point inside the scenario directory.
- It compares: stdout bytes; **stderr bytes**; exit code; and afterwards the output of `git for-each-ref`, `git rev-parse HEAD`, `git status --porcelain=v2 -z` and `git ls-files -s` (each run with the same tool as the case), the set and content of state files in the gitdir (`MERGE_HEAD`, `rebase-merge/*`, `COMMIT_EDITMSG`, …, §4.4), and the working-tree file bytes. Normalisation is limited to temp paths, timing and progress lines, and the program name in hints (§11.6). **Ids are not normalised:** asc must print the same git SHA-1s.
- Allowed differences are listed one by one in `tests/compat/allowed.toml`, each with a reason and the SPEC section that justifies it (for example §2.4.1 checkout conversion). Anything not listed fails.
- **Required coverage:** every row of §11.2; every entry of the native table (§11.5); hooks (§9.4.1), including husky `pre-commit`/`pre-push`/`post-checkout` and commitlint `commit-msg`; worktrees, including Claude Code's create/remove flow with and without hooks; push rejection and upstream messages; in-progress merge, rebase, cherry-pick, revert and bisect; config, aliases and `includeIf`; `.ascignore` precedence and the shadow warning; a lint-staged run with partially staged files, including its pre-commit `git add` being committed (§9.7); `git` inside hooks and node scripts reaching asc with no user shim installed (§9.4.1); a `reference-transaction` hook; external commands and verb precedence (`external-commands.t`: a `git-hello` on PATH, a user alias shadowing an asc-only verb, §11.4); op-log recording, pre-destroy snapshots and `ChangeMeta` writing leaving git output unchanged (`stack-invisible.t`, §20.3; stack verbs get their own `stack_*` tests, not compat cases); and every recorded trace; the full suite also runs with `asc.branchless=true` and `asc.branchless=false`, and the two runs must be byte-identical for local commands (§21.1 BD7; the remote effects of `push` and the `fetch` cloud pull are listed in `allowed.toml`, and `remote:` lines are stripped as other server text is).
- **Gate:** every phase exit in §18 after phase 1 also requires "compat suite green", and every release (O02) requires it on Windows and Linux. Upgrading the pinned real-git version re-records the git side.

### 17.2 Hermetic simulation (`crates/asc-sim`, test-only) [audit: 24-hermetic-simulation]

One process runs N `asc serve` replicas (primary and cache role), M clients, the fake Forgejo and an in-memory store under `turmoil` (simulated network and clock, seeded RNG). A `Faulty<O: Objects>` decorator and turmoil partitions inject added latency, `UNAVAILABLE`, a lost response after a successful write, replica crashes at named points (`before_cas`, `after_cas_before_outbox`, `mid_import`, `mid_land`, `mid_gc_mark`, `mid_gc_sweep`) and daemon kills mid-switch (in-memory `Provider`).

- **Scenarios:** 20 agents pushing and landing concurrently with overlapping edits; webhook storms with duplicates; fetch and push during GC marks and sweeps; replica restart during outbox delivery; client/server version skew; a cache-role failure (upstream must not be stampeded).
- **Invariants checked after every step:** every ref's closure exists; `ref_log` versions strictly increase per ref; each `op_id` has at most one effect set; every committed ref change eventually gets a delivered `post_receive`; land produces linear first-parent histories that contain every accepted change; `mainline`/`path_changes` equal a recomputation; I1–I5 (§14.4) hold; upstream request rate stays within the shed limits.
- A failing seed is printed and replays deterministically. CI runs 200 seeds on PRs that touch `asc-store`, `asc-engine` or `asc-serve`, and 10,000 nightly.

### 17.3 Fuzzing [audit: 24-fuzzing]

`fuzz/` (cargo-fuzz, libFuzzer) has these targets: `envelope_decode` (every kind), `git_commit_parse_canonicalise`, `git_tree_parse`, `git_tag_parse`, `index_read`, `revspec_parse`, `patch_parse` (`apply`), `path_validate` (`Name`, `RepoPath`, `checkout_safe`, casefold), `proto_convert` (every `TryFrom` in `asc-rpc`), `bucket_ref_entry`, `ci_toml`, `projects_toml`, `jsonc`, and one per manifest and lockfile parser (§12A.2). Each asserts no panic and no unbounded allocation (`-rss_limit_mb=2048`); decoders also assert `encode(decode(b)) == b` whenever decode succeeds. CI runs each target for 60 s on PRs that touch its crate and for 30 min nightly. Every crash input is committed under `fuzz/regressions/<target>/` and replayed by `cargo test`.

---

## 18. Roadmap, effort and rollout

Team assumption: 2 Rust engineers, 1 Go engineer (from week 8), and implementation agents (run at effort ≤ medium). Weeks are calendar weeks from start.

| Phase | Weeks | Units | Ships | Exit criteria |
|---|---|---|---|---|
| 1 Local-only asc | 1–5 | U01–U28b (incl. U01b), U20a, U54w | `asc init/add/commit/status/diff/log/branch/switch/merge/rebase/stash/blame/worktree` on local disk, plain mode; asc installable as `git` | compat suite (§17.1) green vs git on the phase-1 scenarios; diff exceptions file empty |
| 1c Local stacks | 5–8 | C01–C15, C51 (after U26/U27c/U27d) | `asc create/modify/absorb/restack/sl/up/down/reword/split/squash/fold/move/reorder/undo/abandon`, op log, change ids (all local, all modes) | compat suite unchanged byte for byte with op-log recording and `ChangeMeta` on (C04); stack tests green |
| 1b Project graph | 3–6 | G01–G08, G11 | `asc affected` / `asc projects` / `asc graph` locally, in all modes, for all four ecosystems | ASC superset check green; `Affected` cold < 2 s |
| 2 Remotes and S3 | 4–6 | U29, U31, U32 | bucket remotes (S3/MinIO/R2/local; single-writer refs) | conformance on MinIO; two workspaces on one machine exchange commits through one bucket (`--refs local`); a clone from another machine fails cleanly (multi-machine sharing needs `asc serve` or U30); compat suite green |
| 3 Git interop | 5–7 | U33–U37, U39, U40, U40b, U24f | import/export, synthetic `.git` for turbo, git-remote mode, hook and tool scenarios, remaining plumbing and history verbs | ASC round trip 0 mismatches; turbo contract on plain mode; compat suite green |
| 4 Server | 7–10 | U41–U51, U65–U71, U72, G09, G10 | `asc serve` (mirror, primary, land, reads, mutations, `/v8`, cache role, seeds, indexes, `Projects`) | M-meta passed; Stage 1 live for pilot; plan job < 10 s; compat suite green |
| 4s Server stacks | 10–13 | C17–C32, C45, C49, C50, C52 (after U47/U66/U68); C33 is v1.x | `Changes` service, `asc submit/sync/stack get/stack merge/share/evolog`, `CommitGraph`, `LineHistory`, `Compare`, pickaxe, `RepoConfig`, stack plugin API | a 5-change stack submits as one `UpdateRefs`, lands 1..3 atomically and restacks 4..5; compat suite green |
| 4b Branchless and commit cloud | 10–14 | B01–B19 (incl. B18a), B21, B22, B24–B26, B28 (B01–B06, B24 after C04/C05; the rest after U47/U48d/U53b/C24); B17, B23, B27 on the Forgejo and CI tracks | user refs and ref policy, commit-cloud upload and sync, `asc hide/unhide/cloud`, branchless `push`, `asc migrate branchless`, `asc serve admin branchless enable` | `push` of an already uploaded stack is one `HasObjects` batch plus one `UpdateRefs`; compat suite byte-identical in both modes; B25 budgets green; an ASC-shaped fixture (117 branches) migrates idempotently |
| 4m Server MCP | 12–15 | M12a, M12b, M13, M14, M18 (after M05, U47, U69, C19, G09) | `asc serve /mcp`: stateless HTTP, OAuth resource server, read and write tools (§22) | `mcp_http`, `mcp_backend`, `mcp_auth` and `mcp_write` tests green; an unreadable repo answers like a missing one |
| 4c Server code intelligence [audit: codeintel-folded] | 12–17 | I01–I17, I22, I29 (after U47, U43c, U69, U44, C19) | `CodeIntel` service: exact search, symbols, vectors, hybrid ranking; `asc search/sym/semantic`; metrics | `scope_oracle`, `ann_acl`, `codeintel_acl`, `codeintel_fork` and `codeintel_cli` tests green; gates of §23.9 measured by I27 |
| 4c-MCP | 15–17 | I21 (after I16, M15, M08) | `search` tool backed by `CodeIntel`; `search.v1` advertised | `search_codeintel` test green |
| 5 Daemon and ProjFS | 8–12 | U52–U58, U73 | virtual worktrees on Windows; plugin API | Windows exit: worktree add < 1 s; turbo hashes equal; 0 hydrations beyond manifests; daemon idle RSS ≤ 64 MiB; compat suite green |
| 5m Local MCP | 12–16 | M01a–M11, M15–M17 (after U73, C31, C24, C25) | the asc JSON profile for `--json`, `asc mcp` over stdio, `asc mcp install`, the generated tool reference | stdio acceptance tests green in local-only mode; default `tools/list` ≤ 6,000 tokens; idle RSS ≤ 24 MiB (M16) |
| 5c Daemon code intelligence | 13–18 | I18–I20 (after U53b, U73) | daemon overlay, degraded local index, opt-in local embeddings | idle RSS ≤ 64 MiB after the reader closes; search works with `--store` local-only |
| 5p Plugin bridge [audit: ide-plugins-folded] | 16–19 | P01a–P03, P45, P46, P57 (after U50, U52, M02, M09, U73, C31) | `asc api --stdio`, `Workspace.ReadFile`, `reorder --todo`, `split --hunks`, the `--json` forms of the plugin verbs | `bridge`, `bridge_run`, `bridge_auth`, `plugin_api_read`, `plugin_json_verbs` tests green; the argv injection corpus is rejected |
| 6 CI | 10–12 | U61–U64, U75 | `asc/actions`, runner alias, host warmer, seeds | typical PR ≤ 8 runner-minutes; compat suite green |
| 7 FUSE and submodules | 12–14 | U59–U60 | Linux virtual, sdk composition | WSL contract green; compat suite green |
| 8 Forgejo Stage 2 | 8–15 | K01–K09 | asc is truth | rollback drill passed on staging; compat suite green |
| 9 Forgejo Stage 3–5 | 15–22 | K10–K15, K17, R01–R02 | full storage replacement; relaxed approvals; runner fork | Forgejo suite green on `asc`; a dual week with 0 mismatches; compat suite green |
| 9s Forgejo stacks | 18–24 | C34–C43, C46–C48 (after K17); C44 is v1.x | stacked PRs, stack panel, versions/interdiff, stack merge and queue, inbox, commit graph page, badges | `asc-sim` lands 20 stacks concurrently with no lost or duplicated change; Forgejo suite green |
| 9m Forgejo MCP | 18–24 | M20–M26, M28 (after K07, K14, C34–C39); M27 is v1.x | `/api/mcp`: PRs, reviews, issues, Actions, inbox; OAuth with audience binding; `lockdown` | the Forgejo `go test` selectors green; an IDE or agent host signs in by OAuth or PAT |
| 9c Forgejo code search | 18–24 | I23–I26 (after K10, I16) | search page, symbol panels, `/explore/code`, API | the Forgejo `go test` selectors green; fixture secrets absent from HTML and JSON |
| 9p Forgejo plugin support | 18–26 | P04–P07, P58 (after K06, K07, K14, M24, M26, C41b) | the read-only RPC proxy, Actions REST, the `asc-ide` OAuth client, `asc-ui` islands, the inbox count | the Forgejo `go test` selectors green |
| 10 VS Code plugin | 18–30 | P08–P35c, P44a, P47a–P47c, P49a, P50, and the `asc-ui` views P13–P23, P56 | the `asc.asc` extension for VS Code, Cursor and VSCodium | the VS Code rows of §24.3 (`VSC v1 = Y`) demonstrated on ASC; P44a budgets met; VSIX smoke test green on VSCodium and VS Code |
| 10j JetBrains plugin | 24–34 | P36–P43b, P44b, P48, P49b, P51–P55 (after P01a–P01c and the `asc-ui` views P15, P17, P19) | one plugin for IntelliJ IDEA, Rider, RustRover, PyCharm and WebStorm | the JetBrains rows of §24.3 (`JB v1 = Y`) demonstrated; `verifyPlugin` green on 2026.1 and 2026.2; P44b budgets met |
| Ops | ongoing | O01–O08, I27, I28, U30 | GC, release/MSI, runbooks, docs, benchmarks, observability, fuzzing, simulation; shared BucketRefs (deferred) | O05 benchmark gate green |

G09–G10 ship with Phase 4, and U63 uses them in Phase 6.

**Milestone M-meta (metadata first, week ≈ 9, before any virtual workspace ships) [audit: 21-metadata-first, roadmap-and-stale-text]:** `asc serve` in mirror mode (§8.6) for asc/dds and asc/soma answers `Version`, `ListRefs`, `Resolve`, `GitMap`, `Log`, `MergeBase`, `IsAncestor`, `Diff`, `Blame`, `LastCommits`, `ReadChangeset`, `ReadTree`, `ReadFile`, `Stat` and `Affected` (`FindLanding` follows with U66). Webhooks keep it current. The CI plan job (§12.2) and automated tools use it with no checkout. M-meta needs no `GetObjects`/`PutObjects`/`UpdateRefs` clients, no daemon and no CLI beyond `asc serve`. Exit: for 500 random (rev, path) pairs from ASC history, every RPC output equals the matching git CLI output (`log --format=%H`, `merge-base`, `diff --name-status`, `blame --porcelain`, `ls-tree`, `cat-file`).

**Pilot and rollout [fix: missing-from-all]:** (1) the agent fleet first (the heaviest worktree users; ephemeral worktrees), (2) 2–3 volunteer developers, (3) all developers, then (4) CI.

Success metrics per cohort: disk per worktree (target < 50 MiB before builds), `worktree add` p95 < 1 s, `status` p95 < 100 ms, hydrations per workspace per day, turbo cache hit rate unchanged or better, PR wall time, and support issues per week. A cohort advances when the metrics hold for one week.

---

## 19. Open decisions for the user, and spikes

**User decisions (defaults in force until decided):**

1. Fold `asc/soma` into `dds`? Default **no**: the sdk is composed read-only (§10.6).
2. Commit `ops/artefacts` (about 200 GiB, sha256-addressed in Garage) as asc large files? Default **no**. It would be ordinary chunked content (§2.4).
3. Flip displayed ids to asc (Stage 6)? Default **no**.
4. Quota semantics: logical size (default) vs namespace-unique size.
5. Show asc-stored Actions artifacts in Forgejo's run UI (a K-patch)? Default **no**: `asc ci artifact` and the forked `download-artifact` are the access path (§12.7) **[audit: actions-cache-artifact-underspecified]**.
6. Logical dedup across a fork network in v1 (the checklist lists it as v1)? Default **no**: forks get a server-side physical copy (`CopyObject`/reflink; Garage dedups blocks), and per-repo namespaces close the confirmation attack. ASC has one repository and no forks, so v1 saves nothing; it is a measured v1.x item (§3.3) **[audit: fork-dedup-deviation-unsigned]**.
7. Branchless defaults to confirm (in force until decided): `git push` creates **draft** PRs (`asc.push.review = draft`), and listing another user's cloud workspace needs repo **write** (`[cloud] list_others = "write"`, §21.2.5) [audit: branchless-folded].

**Spikes (each 1–2 days, in this order; owner = the unit that depends on it):**

| # | Spike | Blocks |
|---|---|---|
| S1 | Enable ProjFS here; a 1k-file provider. Confirm: hydration keeps `PRJ_FILE_BASIC_INFO` times; turbo's gix-index stat check matches with `CreationTime=ChangeTime=LastWriteTime=T`; Defender and the indexer behaviour with the deny list; `PRJ_UPDATE_ALLOW_TOMBSTONE` removal; `PRJ_UPDATE_ALLOW_DIRTY_DATA` on Full files; directory-rename pre-hydration; `ReadDirectoryChangesW` events for switch adds/updates/deletes in enumerated directories | U56/U57 |
| S2 | ASC round-trip import (all 5,502 commits) | U35 |
| S3 | MinIO, R2 and Garage `If-None-Match` behaviour (probe outcomes) | U29/U30 |
| S4 | Turbo 2.10 with the synthetic `.git` (common dir at `repos/<id>/.git`, shared cache path) and the shim first on PATH: record every git invocation turbo makes (`GIT_TRACE`), confirm its index reading needs no `objects/`/`refs/` | U36, U61p |
| S5 | FUSE passthrough in WSL 6.18 | U59 |
| S6 | Dev Drive (ReFS) and ProjFS together | dehydration policy |
| S7 | Claude Code `WorktreeCreate`/`WorktreeRemove` hook JSON field names and the stdout contract, against the pinned version | U54w |
| S8 | *(dropped with the No-IDE decision: there is no promisor projection)* | – |
| S9 | bun.lock (bun 1.2 and 1.4) nested package keys, how each workspace member's dependency names resolve, and how gitlinked members appear in `workspaces`, checked against ASC's bun.lock | G03 |
| S10 | Which packages turbo 2.10 marks for root-file, root `package.json` and `bun.lock` changes on 50 ASC commits, so the superset test's expected differences are understood | G11 |
| S11 | ProjFS: a junction (or Developer-Mode directory symlink) inside a virtualization root pointing to another root, for nested worktree paths | U57 |
| S12 | Shim fall-through overhead on Windows (`CreateProcess` pass-through) against the 5 ms p95 budget | U20a |
| S13 | Whether Forgejo Actions fires `pull_request` for a draft PR (not documented); if not, patch the fork (B17) or default `asc.push.review` to `publish` on repos that need CI on every push | B17, B27 |

The IDE-plugin spikes are PLAN units, not S-numbered: P35b (Cursor and VSCodium engines), P50 (MCP OAuth from IDE clients), P51 (what degrades without a `VcsLogProvider`; the comments API), P52 (Git4Idea and the synthetic `.git`), P53 (partial-line commits), P54 (the `jcef` module) and P55 (OAuth loopback). See §24.9 [audit: ide-plugins-folded].

---

## 20. Stacked changes and stacked PRs [audit: stacks-design-not-folded, stacks-design-exists-not-folded]

Normative from v1.3 (checklist decision 2026-10-22, "stacked commits and stacked PRs are core"). This section folds in decisions D1–D12, the data model, the commands, the server and Forgejo design and the plugin API of `docs/research/stacks.md`. That file stays the background: its feature matrix (§1, which product has which feature and why each row is in or out of scope) and its review notes are not repeated here. PLAN units are **C01–C52** (stacks.md's S01–S52, renamed so they cannot be confused with the spikes S1–S12 of §19; C16 is withdrawn and not reused).

Fixed rules: git commands keep git's exact semantics, output and exit codes (§11); stack features are additive verbs; GitLens/GitKraken-style features are data APIs rendered by ASC's IDE plugins (§24) and the Forgejo fork; the default branch is `master`; there are no whole-repo scans; there is no LFS.

### 20.1 Decisions

| # | Decision | Why |
|---|---|---|
| D1 | **The unit of review is the change (one commit).** A stack is the chain of draft changes from a trunk to a head. Branches are optional labels on changes. | jj, Sapling, Gerrit, spr and ghstack all use this model. Graphite's "one commit per branch" convention is the same model with a branch name on every commit. |
| D2 | **Change identity is a `ChangeMeta` sidecar object.** It is content-addressed (`m/<meta id>`, BLAKE3-verified like every other object) and names the changeset it describes. It is not part of the hashed changeset and not part of the git commit. | Any identity that is hashed or rendered into the commit changes the git SHA of every commit asc makes. That fails the compat gate ("ids are not normalised", §17.1) and breaks export determinism (§2.1). Content addressing keeps §0 decision 5 (every read is verified) true for the sidecar too. |
| D3 | **Stack parents come from the commit graph plus change ids.** No per-branch parent metadata is stored. | There is nothing to drift or repair, so `gt track` is unnecessary. Obsolete and landed parents are found through the change index and `land_map` (§8.9). |
| D4 | **No existing git command changes behaviour, under either `git` or `asc`, with or without configuration.** Stack features are new verbs only. Automatic restack after a git rewrite is not offered. | asc's git commands are the git commands, and the checklist rule ("never change what an existing git command does") has no opt-in exception. Re-adding an opt-in needs the user's explicit sign-off. **Recorded amendment (user decision "branchless by default", §21.1 BD7):** local commands are unchanged in every mode; the remote effect of `push` (what the server stores for a non-shared branch, whether the stack is submitted as draft PRs, whether tags are accepted) and the best-effort cloud pull of `fetch` depend on `asc.branchless` and `repos.branchless`. Their exit codes, stderr and stdout ref lines stay git's, and `asc.branchless=false` or `repos.branchless=false` restores git's remote semantics. |
| D5 | **When argv[0] is `git`, a stack verb runs only where git would print "not a git command".** A user alias or a `git-<verb>` executable on PATH wins. The verb match is exact and happens before `help.autocorrect` (§11.4). | git-extras ships `git undo`, `git sync` and `git squash`, and gitext ships `git stack`. Existing setups keep working, and git's typo handling stays byte-identical. |
| D6 | **The local operation log groups the existing `ref_log` rows by `op_id`.** It adds redacted argv, the workspace and an optional working-copy snapshot, bounded to 1,000 ops or 90 days. `asc undo` covers every ref-moving command, git ones included, and restores the tracked working copy for every command that destroys it (§20.3). Untracked and ignored files that `clean` deletes are a documented gap. | jj/Sapling-style undo for about 200 B per command, recorded invisibly. |
| D7 | **Restack runs in memory.** It uses a metadata rebase when paths are disjoint (§4.4, §4.5). Only a conflicted change is materialised, in git's own `rebase-merge/` state. | It is instant, does not disturb the working tree, and leaves `git status` and `git rebase --continue` behaving as git. |
| D8 | **A stacked PR is stored with the trunk as `base_branch`, and its diff base (`merge_base`) is its own parent commit.** The PR is found by change id through the AGit topic `asc-<change>` (§13.5). Users see the checklist behaviour: the base shown is the parent PR, and it becomes the trunk when the parent merges, with no retarget step. | No branch per change, no synthetic base branch (ghstack/spr) and no retarget dance. A metadata rebase keeps the PR's diff unchanged. The commits tab and diff show only the change because both key off `merge_base` (§20.5). |
| D9 | **The asc server owns forge-neutral change facts** in a new `Changes` service: the index, versions, stack shape, mergeability, overlaps and interdiff. **Forgejo owns social state**: reviews, inbox and queue. Forgejo pushes a summary back to asc (`SetReviewState`). | Scion's API-first rule, and `repo.proto` stays forge-neutral (§7). The CLI and plugins never call a forge API for stack data. |
| D10 | **A stack lands as one `UpdateRefs(LAND)` call with the prefix's top change as the new tip.** The land is atomic, is a pushrebase (§8.5) and takes no global lock. The "merge queue" is Forgejo's ordered merge-when-ready built on this primitive (§12.6). | Post-merge testing stays as in §12.6. No speculative CI and no batching. Revalidation runs only when the land needed a content merge. |
| D11 | **After a parent lands, the server restacks open children (`RestackChanges`).** An approval survives when the change's own `changes` list is unchanged. | Graphite's server-side restack. The "patch unchanged" check is O(1) on Bonsai changesets. |
| D12 | **GitLens/GitKraken features are delivered as data:** Workspace IPC plus `asc.v1` RPCs (`CommitGraph` with lanes, enriched `Blame`, `LineHistory`, `Compare`, `Worktrees`, `Stacks`, `Conflicts`), §20.6. ASC's plugins and the Forgejo fork render them. Everything is free. AI review and chat features stay out of core; agent interfaces (MCP, §22) are core [audit: mcp-folded]. | The most-loved features (blame, history, graph) are served from derived data. |

### 20.2 Stable change identity

A field in `Changeset`, a Gerrit `Change-Id:` trailer and an `asc.change` extra-map key were rejected: each is hashed into `ChangesetId` or rendered into the git commit, so every asc commit's git SHA would differ from git's. The chosen design is a content-addressed sidecar that names its changeset and is never rendered.

```rust
pub struct ChangeId([u8; 16]);                 // random (OsRng) at creation
pub struct MetaId([u8; 32]);                   // BLAKE3 of the ChangeMeta envelope
pub struct ChangeMeta {
    pub version: u8,                           // 1
    pub cs: ChangesetId,                       // the changeset this describes (binds the object to one commit)
    pub change: ChangeId,
    pub preds: Vec<ChangesetId>,               // sorted, deduplicated; empty for a new change
    pub op: RewriteOp,
}
pub enum RewriteOp { New = 0, Amend = 1, Rebase = 2, Absorb = 3, Split = 4, Fold = 5, Squash = 6, Land = 7, Restack = 8, Import = 9 }
```

**Storage.**
- Envelope kind 6 (§2.2) under key `m/<meta id>` (§3.1), `OBJ_KIND_CHANGE_META = 9` (§7). `put` and every read verify the BLAKE3 of the envelope (§0 decision 5, §3.2), so caches cannot serve a corrupt one. Every tier caches `m/` objects as immutable. A meta is collected with its changeset (§3.6).
- The map `cs → meta id` is index data, not object data: `changes.meta` locally (§20.3) and on the server (§20.4), and `LogEntry.meta` on the wire. The first meta indexed for a changeset wins. A second, different meta for the same changeset (only possible for byte-identical commits made independently) is stored but not indexed.
- **Validation on ingest (§8.4).** The server accepts a meta only if `meta.cs` is a changeset in the same push closure (or already stored), so a meta cannot be attached to someone else's commit. `preds` may name any stored changeset.

**Rules.**

| Operation | Change id of the result | `preds` |
|---|---|---|
| `commit`, `asc create` | new | [] |
| `commit --amend`, `asc modify`, `asc absorb` (each target) | same | [old] |
| `rebase`, `rebase -i` pick/reword/edit, `pull --rebase`, restack, server `LAND`/`Rebase`/`Merge(REBASE)`/`RestackChanges` | same | [old] |
| `rebase -i` squash/fixup, `asc fold`, `asc squash` | the earliest folded change's | [all folded] |
| `asc split` | The piece that keeps the original message keeps the id; the other pieces get new ids. | [old] on every piece |
| `cherry-pick`, `revert`, merge commits, `stash` | new (a copy is a new change); stash snapshots get none | [] |
| `Merge(SQUASH)` of a PR submitted with asc | the PR's change | the sources |
| git import (§14.1) | jj `change-id` header, decoded; else Gerrit `Change-Id` trailer → `blake3_derive("asc v1 change gerrit", scope \|\| 0x00 \|\| value)[..16]`, where `scope` is the first import ref (in sorted ref order) that reached the commit, recorded in the written meta so later imports keep it; else none (the migration and branchless-push tier of §21.6.2 adds a derived id). A commit reachable from an immutable ref is never obsolete or divergent (§20.3). | [] |

**Display.** The full form is 32 reverse-hex letters `k`–`z` (jj's alphabet: hex digit d becomes `'z' - d`); the short form is 12 letters. They can never be confused with the hex ids of §2.1. New verbs resolve a positional argument in this order: exact branch name, git revspec (`~` and `^` keep git's meaning), unique change prefix (at least 4 letters). A change id is read with `asc change-id <rev>` (`--short` prints 12) or `asc stack info --json`. Git commands gain no flags (§11.5).

**Transport.** Push: the push closure (§6.3) includes the `m/` objects of the new changesets. Fetch: `LogEntry.change` (field 8) and `LogEntry.meta` (field 10) carry the ids, so fetch makes no extra round trip. Bucket remotes store `m/` like any other object. Rolling upgrade: the server advertises `changes.v1` in `Version.features`, and clients upload `m/` objects only after seeing it, so old readers never meet the new key kind (§16.2). A plain git remote (Stage 0) carries no `m/` objects (§20.7 fallback).

### 20.3 Stack derivation, operation log and local state

**Stack derivation (no stored parent metadata).**

| Term | Definition | Cost |
|---|---|---|
| trunk tips | For each `asc.stack.trunk` value (default: the repo default branch, `master`), `refs/remotes/<remote>/<t>` if present, else the local branch | O(trunks) |
| immutable | Ancestors of any trunk tip or any ref matching `asc.stack.immutable` (default `refs/remotes/*/release/*`). Tags are not checked. | `is_ancestor` through generations and skip lists (§4.3), O(trunks + patterns) per change |
| draft change | A non-immutable changeset that has a `ChangeMeta` | O(1) lookup |
| heads | Local branches, plus every workspace `HEAD`, plus (with `--remote`) changes in my open reviews | O(local refs) |
| stack of a head | First-parent walk from the head while the changeset is a draft. A merge changeset ends the walk; stack verbs never rewrite merges. | O(stack length × log n) |
| stack trunk | The trunk whose tip has the highest-generation merge base with the head | O(trunks × log n) |
| parent change | The change of `parents[0]` when that is a draft; otherwise the change is the stack bottom | O(1) |
| branch parent (Graphite view) | The nearest ancestor change that carries a branch label | O(stack) |
| obsolete | Changeset X is obsolete when a visible changeset with the same change has X in its `preds` closure. "Visible" means reachable from a head or from the latest op-log view. A changeset reachable from an immutable ref is never obsolete. | local index |
| needs restack | The parent is obsolete, or the parent's change has landed (`landed_as` from the change index or `land_map`) | O(stack) |
| behind | The stack base is not the trunk tip (reported, not an error) | `is_ancestor` |
| divergent | One change has more than one visible successor head. Changesets reachable from an immutable ref are ignored, so Gerrit cherry-picks that share an id across branches are not divergent. | local index |
| frozen | A row in `stack_frozen`. Frozen changes are never rewritten locally. | O(1) |

A revset subset exists only under `-r/--revset`: `stack()`, `draft()`, `trunk()`, `mine()`, `mutable()` and `git(<spec>)` with `|`, `&` and `~` (difference). Positional arguments are never revsets, so git's `HEAD~2` keeps its meaning. Stack verbs never rewrite a branch that another workspace has checked out (§9.3); they skip it and report it.

**Operation log (local, bounded).**
- **One op per command.** Every ref-moving command already writes `ref_log` rows with an `op_id` (§3.4) through the local ref chokepoint (§9.4.1). The op log adds one row per `op_id`, in the same `state.db` transaction, git commands included. Recording changes no output. `argv` is stored redacted: URL credentials (`scheme://user:pass@`) become `scheme://***@`, the value of every `-c key=value` becomes `***`, and the value of any `--password`, `--token` or `--auth*` option is dropped.
- **Working-copy snapshot.** Before a verb that rewrites the working copy (`modify`, `absorb`, `split`, `restack` when it pauses, `undo`) **and before every git command that can destroy tracked changes** (`reset --hard`, `checkout -f`/`switch -f`/`--discard-changes`, `checkout <paths>`, `restore` of the worktree, `stash drop` and `clear`, `rebase --abort`, `merge --abort`), the dirty tracked files and the staging rows are saved as an `asc.snapshot` changeset in `store/`, as `stash create` does. Cost O(dirty files). **Documented gap:** untracked and ignored files removed by `clean` (or replaced by `checkout`/`switch`) are not captured, and `asc undo` says so in a `hint:` when the op's argv is one of these.
- **`asc undo`** applies the inverse of the op's `ref_log` deltas, with a CAS against each ref's recorded new value, and restores `HEAD` and, if present, the snapshot. It refuses when a ref has moved since the op, or when it would move a branch that another workspace has checked out, unless `-f`. Undo is itself an op, so `asc redo` undoes the undo. Remote effects (push, submit, land) are never undone; undo prints a `hint:` naming them.
- **Bounds.** Ops older than `asc.opLog.maxDays` (90) or beyond `asc.opLog.maxOps` (1,000) are pruned. `ref_log` rows follow git's `gc.reflogExpire`. A snapshot referenced only by pruned ops is removed from `store/` by `asc gc`. `asc.opLog=false` disables recording.

**Sink contract [audit: rewrite-sink-contract-insufficient-and-double-owned].** The commands that write changesets call the sinks themselves; C04 only supplies the implementations.
- `RewriteSink::record(&self, new: ChangesetId, preds: &[ChangesetId], kind: RewriteOp, op: OpId)` is called once for every changeset a command writes, except stash snapshots. `OpId(Uuid)` is the newtype of `RefTxn.op_id`.

  | Command | `preds` | `kind` |
  |---|---|---|
  | `commit`, `cherry-pick`, `revert`, merge commit | `[]` | New |
  | `commit --amend` | `[old]` | Amend |
  | `rebase`, `rebase -i` pick, reword and edit, `pull --rebase` | `[old]` | Rebase |
  | squash, fixup, fold | all folded changesets | Squash (Fold for `asc fold`) |
  | each split piece | `[old]` | Split |

- C02's rules table (§20.2) maps `(kind, preds)` to keep or mint a change id. Stack verbs pass their own `RewriteOp` (Absorb, Restack, …).
- **`DraftHeadSink` [audit: branchless-folded]:** C04's sink implementation also calls B03's `DraftHeadSink` next to `RewriteSink::record`, inside the same transaction, to maintain `draft_heads` and `cloud_queue` (§21.2.2). It runs in both modes and is invisible.
- `OpSink::snapshot_wc` runs before every command listed in "Working-copy snapshot" above, including `merge --abort` and `rebase --abort`.

**Local state (`state.db`, §9.2).**

```sql
changes(repo_id, cs BLOB, change BLOB, meta BLOB /*MetaId*/, op INT, PRIMARY KEY(repo_id, cs)) + INDEX (repo_id, change)
change_preds(repo_id, cs BLOB, pred BLOB, PRIMARY KEY(repo_id, cs, pred)) + INDEX (repo_id, pred)
op_log(repo_id, seq INTEGER PRIMARY KEY, op_id BLOB UNIQUE, ws_id TEXT, at INT, argv TEXT /*redacted*/, kind TEXT,
       head_before BLOB, head_after BLOB, wc_snapshot BLOB NULL, undoes_seq INT NULL)   -- ref deltas = ref_log rows with op_id
stack_frozen(repo_id, change BLOB PRIMARY KEY)
review_cache(repo_id, change BLOB PRIMARY KEY, review BLOB /*postcard ReviewState*/, fetched_at INT)   -- TTL 60 s
```

Config is git-format under `asc.stack.*` (§11.1; `stack.*` belongs to other tools such as git-stack): `trunk` (multi-valued), `immutable`, `exclude` (ref globs; replaces Graphite's `untrack`), `branchTemplate` (default `{user}/{slug}`) and `submit.draft`. A paused stack op keeps its remaining plan in `<gitdir>/asc-stack-op.json`, next to git's state files (§4.4).

### 20.4 Server tables and derived data

```sql
changes(ns BLOB, cs BLOB, change BLOB, meta BLOB, op SMALLINT, preds BLOB, PRIMARY KEY(ns, cs)) INDEX changes_by_change (ns, change)
change_versions(ns BLOB, change BLOB, v INT, cs BLOB, parent_change BLOB NULL, at BIGINT, actor TEXT, op_id BLOB,
                PRIMARY KEY(ns, change, v))                                  -- GC roots (§3.6), never in ListRefs
review_state(ns BLOB, change BLOB, state BLOB /*ReviewState*/, updated BIGINT, PRIMARY KEY(ns, change))
open_paths(ns BLOB, path_key BLOB /*16 B, as path_changes*/, change BLOB, PRIMARY KEY(ns, path_key, change))
mergeable(ns BLOB, head BLOB, trunk_tip BLOB, state SMALLINT, conflicts BLOB, computed_at BIGINT, PRIMARY KEY(ns, head))   -- one row per head, overwritten
```

- `changes` is written in `UpdateRefs` step 4 (§8.3) and in `Ingest` (§8.7) for every new changeset whose closure carries a valid `m/` object (§20.2). Land, `Merge(REBASE|SQUASH)` and `RestackChanges` write rows for the rewritten changesets in the same transaction as `land_map` (§8.9).
- **The server writes meta for every changeset it creates [audit: stack-server-and-sync-hooks-unowned].** It writes a `ChangeMeta` object and a `changes` row for each: `Commit` New; land, `Rebase`, `Merge(REBASE)` and `RestackChanges` Rebase or Restack with `[old]`; `Merge(SQUASH)` the PR's change with the sources as `preds`. The write happens in the transaction that writes `land_map` (§8.9), or in step 4 for `Commit` and `Ingest`, through the `IngestObserver` hook of the ingest pipeline. The §8.4 closure check (`meta.cs` is in the push closure) runs in the same hook. `Log` and `Blame` fill `LogEntry.change`, `meta` and `stats` through a `LogEntryEnricher` hook.
- `landed_as(change)` is a lookup in `changes_by_change`, filtered to changesets on a land branch's `mainline` (§5.1): one indexed join, never a scan of messages (Scion lesson 23).
- `open_paths` is maintained when a version is published or a PR closes, from the head changeset's own `changes` keys (relative to its parent), so no diff is needed. `Overlaps` costs O(paths × matches) and skips changes of the same stack (through the `parent_change` chain).
- `mergeable` is a plain SQL table (§5): one row per head, overwritten when the head or the trunk tip changes, deleted when the PR closes. It uses the path-disjointness check (§4.5) first and reads content only for intersecting paths, recomputed lazily on request. Forgejo asks only for open PRs whose `open_paths` intersect the new trunk changes.
- `diffstat.v1` (§5) feeds `CommitGraph` stats, visual file history and `Log.with_stats`.

### 20.5 Server model, flows and RPCs

| Concept | asc server (forge-neutral) | Forgejo fork |
|---|---|---|
| Change and versions | `changes` and `change_versions` (GC roots) | PR ↔ change (`asc_change` column) |
| Stack shape | `change_versions.parent_change`, plus `Changes.Stack` | `stack_parent_id` cache for listing; the truth is asc's |
| PR base | – | `base_branch` = the stack trunk (for example `master`) |
| PR diff base | – | `merge_base` = the head's first parent while the parent change is unlanded, else the merge base with trunk. Forgejo's diff, commits tab, review, "viewed" and outdated-comment logic all key off `merge_base`, so the patch and the commit list show only the change. |
| Review / approvals / inbox | `review_state` summary only, written by Forgejo | Owner |
| Landing | `UpdateRefs(LAND)` (atomic prefix), `land_map`, `FindLanding` | Merge button, queue, policy |
| Upstack restack | `RestackChanges` | Triggers it after a merge |

**Forgejo seams that D8 requires (C34, C36):** (1) Forgejo recomputes `merge_base` on every push and base-branch update (`UpdateCommitDivergence`/`getMergeBase`); C34 patches that path so a PR with `asc_change` uses the rule above and is never recomputed from the base branch. (2) `POST /repos/{o}/{r}/pulls/{n}/merge` (and the web merge button) on a PR with an unlanded parent PR returns 409 `stack-parent-unmerged` with a link to the stack merge (C36); "Merge stack (N)" is C38.

**Flows.**
1. **Submit.** `asc submit` sends one `UpdateRefs` whose updates are `refs/for/<trunk>/asc-<change>`, bottom to top, plus push options `asc.draft` and `asc.reviewers`. Pipeline step 4 (§8.3) ingests the closure once, including the `m/` objects. Per ref, in order, asc calls Forgejo `proc-receive` (§13.5). The fork finds the PR by `(repo, change)`. **Who may update it:** the PR's author, a repo admin or owner, or any user with write access when the PR has "allow edits from maintainers" ticked; anyone else gets `not allowed to update change <short>` (`CHANGE_NOT_ALLOWED`), so a forged `preds` cannot replace someone else's PR head. It creates the PR (title and body from the change's message) or records a new version, and sets `stack_parent_id` from the parent change's PR. asc moves `refs/pull/<n>/head` with `SKIP_PRE` and appends `change_versions(v+1)`. **Lease:** a version is accepted only if the server's latest version of that change is in the new changeset's `preds` closure and every `m/` object in the push names a changeset in the same closure; otherwise `CHANGE_DIVERGED`, unless `--force` (which still needs the authorisation above).
2. **Review.** The PR shows the stack panel, "version N of M", interdiff against any earlier version, and "changes since your last review" (the version of the reviewer's last review `commit_id`). Comments outdate through Forgejo's existing line mapping.
3. **Land prefix 1..N.** Forgejo checks that every PR in the prefix is approved and green against the trunk's branch protection, then calls `UpdateRefs(master ← head(N), LAND, expect = the approved heads)`. The whole prefix lands atomically (§8.5); `rewritten` gives each PR's landed changeset, and the PRs are marked merged. Stacks always land rebase-style, one commit per change.
4. **Restack after land.** Forgejo calls `RestackChanges(onto = new trunk tip, items = the open children bottom to top, with update_ref = refs/pull/<m>/head, atomic)`. Each child gets a new version. The rewritten changesets use the **actor** of the call as committer and one **timestamp per call**, both carried in the request, so a retry with the same request yields the same ids. Author and message are kept. `patch_unchanged` keeps approvals even with dismiss-stale enabled. On conflict the child is marked "needs restack", and the author runs `asc sync && asc submit`.
5. **Queue.** "Merge when ready" on PR N enables it for 1..N. A per-target-branch worker processes entries in enqueue order, one land in flight per branch, and always lands the largest ready prefix. Admins can pause a branch. With `asc_stack_queue.revalidate = on_content_merge` (default) the worker lands with `UpdateRefsRequest.no_content_merge` set; the server fails the land with `LAND_NEEDS_CONTENT_MERGE` when §8.5 step 3 would need a content merge, and the worker restacks the stack as new versions and waits for checks again. Path-disjoint lands succeed straight away, and post-merge CI with `green/*` refs covers them (§12.6). `revalidate = off` omits the flag.
6. **Branch-chain PRs** (plain `git push` of branches with base = another PR's head branch; C46) are recognised as stacked. When the parent merges, the base is retargeted to the parent's base, as GitHub does. They are not rebased on the server unless the author ticked "allow server restack".

**RPCs (new `proto/asc/v1/changes.proto`, same port; additive `repo.proto` fields).**

```proto
service Changes {
  rpc ResolveChanges(ResolveChangesRequest) returns (ResolveChangesResponse);  // cheap
  rpc Stack(StackRequest) returns (StackResponse);                             // cheap
  rpc Versions(VersionsRequest) returns (VersionsResponse);                    // cheap, paged
  rpc InterDiff(InterDiffRequest) returns (stream DiffResponse);               // expensive
  rpc Mergeability(MergeabilityRequest) returns (MergeabilityResponse);        // expensive, memoised
  rpc Overlaps(OverlapsRequest) returns (OverlapsResponse);                    // cheap (index)
  rpc RestackChanges(RestackChangesRequest) returns (RestackChangesResponse);  // mutating, x-asc-op-id
  rpc SetReviewState(SetReviewStateRequest) returns (SetReviewStateResponse);  // internal token only
  rpc RequestMerge(RequestMergeRequest) returns (RequestMergeResponse);        // mutating; proxies to the forge's queue
}
// ReviewState is defined in repo.proto (below), so repo.proto's graph rows and blame reviews do not import changes.proto.
message ChangeInfo { bytes change = 1; bytes head = 2; bytes git = 3; uint32 version = 4; bytes parent_change = 5;
  bool parent_current = 6; bytes landed_as = 7; ReviewState review = 8; repeated bytes visible = 9; /* >1 = divergent */ }
message ResolveChangesRequest { RepoRef repo = 1; repeated bytes changes = 2; repeated string revs = 3; }
message ResolveChangesResponse { repeated ChangeInfo changes = 1; }
message StackRequest { RepoRef repo = 1; string target = 2; oneof at { bytes change = 3; string rev = 4; uint64 review = 5; } }
message StackResponse { string trunk = 1; bytes base = 2; repeated ChangeInfo changes = 3; /* bottom → top, forks included */ }
message VersionsRequest { RepoRef repo = 1; bytes change = 2; string page_token = 3; }
message Version { uint32 v = 1; bytes cs = 2; bytes git = 3; bytes parent_change = 4; int64 at = 5; string actor = 6; }
message VersionsResponse { repeated Version versions = 1; string next_page_token = 2; }
message InterDiffRequest { RepoRef repo = 1; bytes change = 2; uint32 from_v = 3; uint32 to_v = 4; DiffFormat format = 5;
  repeated string paths = 6; }
message MergeabilityRequest { RepoRef repo = 1; string target = 2; repeated bytes changes = 3; }
message Mergeable { bytes change = 1; enum State { STATE_DISJOINT = 0; STATE_CLEAN_MERGE = 1; STATE_CONFLICTS = 2;
  STATE_NEEDS_RESTACK = 3; } State state = 2; repeated Conflict conflicts = 3; }
message MergeabilityResponse { repeated Mergeable results = 1; }
message OverlapsRequest { RepoRef repo = 1; string target = 2; bytes change = 3; repeated bytes paths = 4; }
message Overlap { bytes change = 1; ReviewState review = 2; repeated bytes paths = 3; }   // never includes a change of the same stack
message OverlapsResponse { repeated Overlap overlaps = 1; }
message RestackItem { bytes change = 1; string update_ref = 2; RefTarget expect = 3; }
message RestackChangesRequest { RepoRef repo = 1; string onto = 2; repeated RestackItem items = 3; bool atomic = 4; bool dry_run = 5;
  string committer_name = 6; string committer_email = 7; int64 committer_time = 8; }   // defaults: the authenticated actor and the server clock
message RestackChangesResponse { repeated Rewrite rewritten = 1; repeated Conflict conflicts = 2; repeated bytes patch_unchanged = 3; }
message SetReviewStateRequest { RepoRef repo = 1; repeated bytes changes = 2; repeated ReviewState states = 3; }
message SetReviewStateResponse {}
message RequestMergeRequest { RepoRef repo = 1; bytes change = 2; bool now = 3; }   // bottom..change
message RequestMergeResponse { enum State { STATE_QUEUED = 0; STATE_LANDED = 1; STATE_REFUSED = 2; } State state = 1; repeated bytes pending = 2; string reason = 3; }
// The server forwards RequestMerge to the forge through the internal channel that carries proc-receive (§13.5), in the
// other direction, so the CLI never calls a forge API (D9). Forgejo answers from its queue (C39); with no forge attached,
// now=true lands directly and now=false is STATE_REFUSED.
```

**repo.proto, additive (amendment to U41, made by C17) [audit: stack-proto-messages-undefined].** `ReviewState` moves here from `changes.proto`. `OBJ_KIND_CHANGE_META = 9` is already in §7's `ObjKind` (U41).

```proto
message ReviewState { string forge = 1; uint64 number = 2; string url = 3; string state = 4; bool draft = 5;
  uint32 approvals = 6; string checks = 7; string queue = 8; uint32 reviewed_version = 9; int64 updated = 10; }
message FileStat { bytes path = 1; uint32 added = 2; uint32 removed = 3; bool binary = 4; }
message DiffStat { repeated FileStat files = 1; }
// LogEntry += bytes change = 8; DiffStat stats = 9; bytes meta = 10;
// LogRequest += bytes pickaxe = 15; bool pickaxe_regex = 16; bool with_stats = 17; uint32 scan_budget = 18 (candidates, default 10000); string resume_token = 19;
// LogResponse += bool truncated = 4; string resume_token = 5;   // set when the scan budget ran out
// UpdateRefsRequest += bool no_content_merge = 7;               // LAND only
// BlameResponse += repeated ReviewState reviews = 3;            // parallel to commits
// ErrorDetail.code += CHANGE_DIVERGED, CHANGE_FROZEN, LAND_NEEDS_CONTENT_MERGE, CHANGE_NOT_ALLOWED;
enum GraphPreset { GRAPH_PRESET_ALL = 0; GRAPH_PRESET_CURRENT = 1; GRAPH_PRESET_SMART = 2; GRAPH_PRESET_STACK = 3; }
message CommitGraphRequest { RepoRef repo = 1; repeated string include = 2; repeated string exclude = 3; GraphPreset preset = 4;
  bool lanes = 5; bool stats = 6; uint32 limit = 7; string page_token = 8; }
message GraphEdge { uint32 to_column = 1; bytes parent = 2; }
message GraphRow { LogEntry entry = 1; repeated string refs = 2; uint32 column = 3; repeated GraphEdge edges = 4;
  bool overflow = 5; ReviewState review = 6; bool wip = 7; string ws_id = 8; }
message CommitGraphResponse { repeated GraphRow rows = 1; string next_page_token = 2; }
message LineHistoryRequest { RepoRef repo = 1; string rev = 2; bytes path = 3; uint32 start_line = 4; uint32 end_line = 5;
  uint32 limit = 6; uint32 scan_budget = 7; string resume_token = 8; }
message CompareRequest { RepoRef repo = 1; string a = 2; string b = 3; bool three_dot = 4; uint32 limit = 5;
  string ahead_page_token = 6; string behind_page_token = 7; }
message CompareResponse { bytes merge_base = 1; uint64 ahead = 2; uint64 behind = 3; repeated LogEntry ahead_entries = 4;
  repeated LogEntry behind_entries = 5; string ahead_next = 6; string behind_next = 7; repeated PathChange files = 8; }
message Autolink { string prefix = 1; string url = 2; }
message RepoConfigRequest { RepoRef repo = 1; string rev = 2; }
message RepoConfigResponse { repeated Autolink autolinks = 1; }
// service Repo += rpc CommitGraph(CommitGraphRequest) returns (stream CommitGraphResponse);  // §20.6; not the project graph of `asc graph`
//                 rpc LineHistory(LineHistoryRequest) returns (stream LogResponse);          // §20.6; takes scan_budget
//                 rpc Compare(CompareRequest) returns (CompareResponse);                     // §20.6
//                 rpc RepoConfig(RepoConfigRequest) returns (RepoConfigResponse);            // autolinks, review.toml at a rev
```

**workspace.proto, additive (C31; `KIND_STACK = 8` and `WatchEvent.change = 8` too).**

```proto
rpc Stacks(WsStacksRequest) returns (WsStacksResponse);
rpc Worktrees(WsWorktreesRequest) returns (WsWorktreesResponse);
rpc Conflicts(WsConflictsRequest) returns (WsConflictsResponse);
rpc CommitGraph(WsCommitGraphRequest) returns (stream CommitGraphResponse);
// WsBlameRequest += bytes buffer = 6; bool ignore_whitespace = 7;
message WsStacksRequest { string ws_id = 1; bool all = 2; bool remote = 3; string revset = 4; }
message StackTrunk { string name = 1; bytes tip = 2; string remote = 3; }
message StackEntry { bytes change = 1; bytes commit = 2; bytes asc = 3; bytes parent_change = 4; repeated string branches = 5;
  bytes subject = 6; Signature author = 7; string state = 8; bool needs_restack = 9; bool frozen = 10; bool empty = 11;
  bool current = 12; string worktree = 13; ReviewState review = 14; string short = 15; }
message WsStack { string trunk = 1; bytes base = 2; uint32 behind = 3; repeated StackEntry changes = 4; }
message WsStacksResponse { uint32 version = 1; repeated StackTrunk trunks = 2; repeated WsStack stacks = 3; }
message WsWorktreesRequest {}
message WsWorktree { string ws_id = 1; string path = 2; string mount = 3; string mode = 4; string head_ref = 5; bytes head = 6;
  uint32 dirty = 7; uint32 staged = 8; uint32 ahead = 9; uint32 behind = 10; int64 ephemeral_until = 11; string locked = 12; string in_progress = 13; }
message WsWorktreesResponse { repeated WsWorktree worktrees = 1; }
message WsConflictsRequest { string ws_id = 1; }
message WsConflict { bytes path = 1; ConflictKind kind = 2; bytes base = 3; bytes ours = 4; bytes theirs = 5; bytes marked = 6; }
message WsConflictsResponse { repeated WsConflict conflicts = 1; }
message WsCommitGraphRequest { string ws_id = 1; CommitGraphRequest query = 2; bool wip_rows = 3; }
```

Rate classes are in §16.3. The `Changes` service follows the §7 lint exceptions.

### 20.6 Plugin API surface

All additive; served by the daemon over IPC (`workspace.proto`), by the server where noted, and by `asc … --json`.

| Need | API | Served from | Efficiency bound |
|---|---|---|---|
| Inline, hover, status-bar and gutter blame; heatmap; CodeLens | `Workspace.Blame` plus `buffer` (unsaved bytes) and `ignore_whitespace`; `BlameResponse.commits[].change`; parallel `reviews[]` | `blame.v1` memo (§4.6) with path-index candidates. Buffer lines are mapped by one line diff against the `HEAD` blob. | O(file) per request after the first derive |
| File history (renames followed) | `Workspace.Log` / `Repo.Log` with `paths`, `follow`, `with_stats` | `path_changes` on land branches (§5.1); otherwise a gen-pruned walk over `changes` keys | O(result + log n) on land branches |
| Line history (`-L`) | `LineHistory(rev, path, start, end, limit, scan_budget)` | Path-index candidates; the range is carried backwards through each candidate's single-file line diff. Stops after `scan_budget` candidates (default 10,000) with `truncated` plus a resume token. | O(min(candidates, scan_budget) × file size) |
| Commit graph, minimap, filters | `Repo.CommitGraph` / `Workspace.CommitGraph(include, exclude, preset: all\|current\|smart\|stack, lanes, stats, page_token)`, named apart from `asc graph` (§12A.9). Rows carry `LogEntry`, refs, `column`, `edges`, `stats`, `change`, `review`; the workspace variant adds a WIP row per worktree. | Generation-ordered pages. Lanes come from the deterministic `asc-engine::commit_graph::lanes`, whose state rides in the page token, capped at 256 lanes (further lanes share one `overflow` column; the token stays under 4 KiB). | O(page); earlier pages never re-read |
| Commit details | `ReadChangeset` plus `ResolveChanges` | existing plus the change index | `cheap` |
| Branch comparison | `Compare(a, b, three_dot, limit)` → `{merge_base, ahead, behind, ahead_page, behind_page, files}` | `MergeBase` with skip lists; mainline positions; `Diff(NAMES)` | O(log n + result) |
| Worktrees view and multi-WIP | `Workspace.Worktrees` → `{ws_id, path, mount, mode, head_ref, head, dirty, staged, ahead, behind, ephemeral_until, locked, in_progress}` | `workspaces` rows plus each journal's dirty count | O(workspaces + changed paths) |
| Worktree management | `asc worktree add\|remove\|lock --json` | §9.3 | `add` < 1 s |
| Stacks / smartlog | `Workspace.Stacks` (= `asc sl --json`) | §20.3, local | O(heads × stack length × log n) |
| Launchpad / PR triage | Forgejo `GET /api/v1/user/asc/inbox` (not an asc RPC) | Forgejo DB plus `review_state` | Forgejo indexed queries |
| Conflicts (merge editor) | `Workspace.Conflicts` → `{path, kind, base, ours, theirs, marked}` | staging stages 1–3 (§9.5) | O(conflicted paths) |
| Graph actions, rebase editor, drag and drop | The stack verbs with `--json` / `--dry-run --json`; `GIT_SEQUENCE_EDITOR` for git's own `rebase -i` | `merge_trees` in memory; `Rebase(dry_run)` remotely | O(changed paths) unless paths intersect |
| Autolinks, web URLs | `RepoConfig(rev)` → `autolinks[{prefix, url}]`; a `web` template in `/.well-known/asc` | `.asc/review.toml` at the revision | `cheap`, cached by `FileId` |
| Events | `Watch` adds `KIND_STACK` (C49): a change rewritten or restacked, a review state changed | the op log and `review_cache` refresh | push-based |
| Status-bar `↓N` [audit: ide-plugins-folded] | `WsStack.behind`: the number of trunk commits not in the stack base, capped at 999 (§24.8.2 A1) | the `Stacks` snapshot | O(min(behind, 999)) with generation numbers |
| Minimap data | the first `CommitGraphResponse` page may carry `total` and `ref_markers[]` (§24.8.2 A2) | mainline positions; omitted when not cheap | O(refs) |
| Transport for IDE plugins | all of the above are reached through `asc api --stdio` (§24.2); the daemon pipe is never opened by Node or the JVM | the bridge | n/a |

### 20.7 Commands

Every verb is asc-only (§11.2). Under argv[0] `git` the §11.4 precedence applies (D5). Every verb takes `--json` (shapes below) and `-n/--dry-run` where it mutates. Exit codes: 0 done or nothing to do; 1 paused on a conflict (as `git rebase`); 2 ambiguous (a fork or several candidates, listed in `candidates` under `--json`); 128 fatal; 129 usage. Under `--json` the exit codes do not change, and the printed result says `status: "paused"` (with `conflicts`) for 1 and `status: "ambiguous"` (with `candidates`) for 2; the MCP server returns both as ordinary results (§22.3.1) [audit: mcp-folded].

| Verb and key flags | Does |
|---|---|
| `asc create [<branch>] [-m <msg>] [-a\|-u\|-p] [--insert] [--onto <rev>] [--no-branch]` | New change on `HEAD` (or `--onto`) from the staged changes; empty if nothing is staged. `--insert` reparents `HEAD`'s children onto it. Names a branch from `asc.stack.branchTemplate` unless `--no-branch`; under branchless mode (§21, the default) `--no-branch` is implied and a branch is made only when `<branch>` is given [audit: branchless-folded]. Runs the commit hooks. |
| `asc modify [-a\|-u\|-p] [-c] [-m <msg>\|-e] [--into <change>]` | Amends the current change (`-c` adds a new change; `--into` amends a downstack change without checking it out), then restacks the descendants. Runs the commit hooks. |
| `asc absorb [-n] [-f] [-p] [--into <revset>]` | Moves each staged hunk (or each unstaged tracked hunk when nothing is staged) to the nearest mutable change it does not commute with (from `blame.v1` line origins), then restacks. Ambiguous hunks and new files stay. Preview unless `-f`. |
| `asc restack [<change>] [--downstack\|--upstack\|--only]` | Rebases each change whose parent is obsolete or landed, or whose base is behind its parent. In memory; pauses on the first conflict. |
| `asc continue [-a]` / `asc abort` | Finishes a paused op (running `rebase --continue` first if needed) or restores the op's before-state |
| `asc sync [--no-fetch] [--no-restack] [-d] [--all-trunks]` | Fetches; fast-forwards local trunks no other workspace has checked out; drops landed changes (by change id and `FindLanding`, never by message); offers to delete merged branches (`-d` deletes them); restacks each stack onto its trunk. Conflicting stacks are skipped and reported. |
| `asc submit [<change>] [-s] [--draft\|--publish] [--reviewer <u>…] [--update-only] [--auto-merge] [--force]` | Pushes bottom..change (or the whole stack with `-s`) as one `UpdateRefs` of `refs/for/<trunk>/asc-<change>` refs, one PR per change (§20.5 flow 1). Refuses if a change needs a restack, is divergent or is frozen. |
| `asc stack merge [<change>] [--now]` | Asks the server to merge bottom..change when ready (`Changes.RequestMerge`). `--now` lands at once if every PR in the prefix is mergeable. Not `asc landed` (§8.9). |
| `asc sl [-s] [-a] [-r] [--remote] [--json]` (also `asc smartlog`) | Draft changes relative to trunk with branch labels, `@`, needs-restack, divergent, frozen, worktree and (with `--remote`) PR number, state, checks and version |
| `asc up [n] [--to <b>]`, `asc down [n]`, `asc top`, `asc bottom` (aliases `next`, `prev`) | Switches by relationship. At a fork it prompts; with `--json` or without a terminal it exits 2, moves nothing and lists the candidates. |
| `asc reword [<change>] [-m <msg>\|-e]` | Edits the message of any mutable change without touching its tree, then restacks. Runs `commit-msg`. |
| `asc uncommit [<change>] [-a\|-p\|<pathspec>…]`, `asc unamend` | `uncommit` moves the change's tree (or the chosen paths) back into the working copy and staging. `unamend` restores the previous version of the current change from its `preds`. |
| `asc abandon [<change>…] [--keep-descendants]` | Drops mutable changes and restacks the descendants onto the parent. The commits stay reachable through the op log. |
| `asc change-id [--short] [<rev>]` | Prints the change id (empty and exit 1 for a commit without one) |
| `asc split [--by-commit\|--by-hunk\|--by-file <pathspec>…]`, `asc squash [-m\|-e\|-n]`, `asc fold [--keep] [--stack]` | Split one change into a stack; collapse a branch's changes into one; fold a change into its parent and reparent its children |
| `asc move --onto <rev> [--source <change>] [--only]`, `asc reorder [--stack]` | Reparent a change and its descendants; editor list of changes bottom to top. `git rebase -i` stays git's. |
| `asc stack rename <new>`, `asc stack delete [--upstack\|--downstack] [-c]`, `asc stack pop` | Branch lifecycle; children are restacked onto the parent; `-c` also closes the PRs |
| `asc stack get <pr\|change\|branch> [-d] [-u] [--unfrozen] [--worktree <path>]` | Fetches a stack from the server, frozen by default. `--worktree` opens it in a new plain worktree (a virtual one once U57/U58 land). |
| `asc stack freeze\|unfreeze [<change>]`, `asc stack trunk [--add\|--remove <b>]`, `asc stack info [<change>]`, `asc stack parent\|children` | Local protection; configuration and relationship reads |
| `asc undo [-n] [-f] [--op <id>]`, `asc redo [--op <id>]`, `asc op log`, `asc op show <op>`, `asc op restore <op>` | §20.3. `--op <id>` makes the call refuse (exit 128, `ErrorDetail.code = STALE_OP` under `--json`) unless that op is the latest in the op log (`undo`) or the latest undone op (`redo`), so an agent can revert only what it just did [audit: mcp-folded]. |
| `asc evolog [<change>] [-p]`, `asc interdiff <change> [<v1> <v2>]` | The versions of a change (local and server) and the diff between versions |
| `asc share [-m <msg>]` | Snapshots the working changes, `Ingest`s them (§8.7), writes `refs/snapshots/<id>` with an expiry (default 30 days, `UpdateRefs.ttl_seconds`, §8.8) and prints `asc:<id>` and a web link. Recipients run `asc stack get asc:<id>`. |
| `asc stack exec [--revset <r>] -- <cmd>` (v1.x) | Runs a command on each change in an ephemeral worktree, results cached by `TreeId` |

**Hooks and signing.** `asc create`, `modify`, `reword`, `split` (once per piece) and `squash`/`fold` (combined message) run git's commit hooks at git's points (§9.4.1): `pre-commit` on the staged tree, `prepare-commit-msg`, `commit-msg`, `post-commit`; `--no-verify` skips `pre-commit` and `commit-msg`. Restack, move and reorder run `pre-rebase` once up front; every rewrite runs `post-rewrite <amend|rebase>` once afterwards with all old/new pairs, plus `reference-transaction`. A restack does not run `pre-commit` or `commit-msg`, as `git rebase` does not. A non-zero hook aborts before any ref moves. Verbs sign as the matching git command does (`commit.gpgSign`, `-S`); the in-memory restack re-signs when `commit.gpgSign=true` and drops the signature when false, as `git rebase` does. A signing failure pauses like a conflict (exit 1) and moves no ref for that change.

**Stage 0 (git-remote mode) fallback.** A plain git remote carries no `m/` objects. Local verbs work on the local `ChangeMeta`. `asc sync` falls back to git semantics: a change is landed when its commit is reachable from the trunk or its patch-id equals a trunk commit's since the merge base; squash merges are reported as "cannot tell". `asc submit`, `asc stack get`, `asc stack merge` and `asc share` exit 128 with `asc: <verb> needs an asc server (not available on a plain git remote)`.

**Stack awareness in existing git commands: none (D4).** `commit --amend`, `rebase`, `rebase -i`, `pull --rebase` and `rebase --update-refs` keep git's semantics and leave descendants untouched (run `asc restack`). Every ref-moving command is recorded in the op log invisibly, and every new commit gets a `ChangeMeta` invisibly (SHAs unchanged). `push`, `status`, `log`, `branch` and `rev-parse` are unchanged.

**`--json` shapes** (every object carries `"version": 1`; ids are git SHA-1s, as in §11.1, plus the change id; field names, ids and enums follow the asc JSON profile of §22.4.2, and a result is always a JSON object, so a list is wrapped) [audit: mcp-folded]:

```jsonc
// asc sl --json  ==  Workspace.Stacks
{ "version": 1,
  "trunks": [{ "name": "master", "tip": "<sha>", "remote": "origin" }],
  "stacks": [{ "trunk": "master", "base": "<sha>", "behind": 3,
    "changes": [   // topological, bottom → top; build the tree from parent_change
      { "change": "<32 k-z>", "short": "<12>", "commit": "<sha>", "asc": "<64 hex>",
        "parent_change": null, "branches": ["jordan/parser"], "subject": "…",
        "author": { "name": "…", "email": "…", "time": 0, "tz": "+0000" },
        "state": "draft|landed|obsolete|divergent", "needs_restack": false, "frozen": false,
        "empty": false, "current": true, "worktree": null,
        "review": { "number": 42, "url": "…", "state": "draft|open|merged|closed",
                    "approvals": 1, "checks": "pending|success|failure|none",
                    "queue": "none|queued|landing", "version": 3, "reviewed_version": 2 } } ] }] }

// every mutating verb
{ "version": 1, "op": "<op id>", "status": "ok|nothing|paused|ambiguous",   // paused: conflicts; ambiguous: candidates
  "rewritten": [{ "change": "…", "old": "<sha>", "new": "<sha>" }],
  "refs": [{ "name": "refs/heads/…", "old": "<sha>", "new": "<sha>" }],
  "skipped": [{ "change": "…", "reason": "checked-out-elsewhere|frozen|conflict|merge|immutable" }],
  "conflicts": [{ "change": "…", "paths": ["…"] }],
  "candidates": [{ "change": "…", "subject": "…" }],
  "reviews": [{ "change": "…", "number": 42, "url": "…", "created": false, "version": 4 }],
  "hints": ["…"] }

// asc op log --json   (argv is already redacted, §20.3)
{ "version": 1, "ops": [{ "op": "…", "seq": 17, "at": 0, "ws": "…", "argv": ["git", "rebase", "master"],
   "refs": [{ "name": "…", "old": "…", "new": "…" }], "snapshot": null, "undone": false }] }

// asc evolog --json
{ "version": 1, "versions": [{ "version": 3, "commit": "<sha>", "op": "amend|rebase|…", "at": 0, "actor": "…", "preds": ["<sha>"], "published": true }] }
```

### 20.8 Forgejo UI (rendered from the RPCs above; §13.3 C34–C48)

| Surface | Content |
|---|---|
| PR page | Stack navigator (bottom to top; state, checks and approvals per PR; keys to move between them), "needs restack" banner with a server-restack button, "Merge stack (N)" or the mid-stack block, version dropdown with interdiff, "since my last review", overlap and mergeability badges |
| Repo | Commit graph page over `CommitGraph`; blame and file history (existing pages, via `ascbackend`); stack compare view (`Compare` between two versions or two stacks) |
| Inbox `/pulls/inbox` and `GET /api/v1/user/asc/inbox` | Needs your review, returned to you, ready to merge, blocked, waiting, drafts, queued or recently merged. Whose turn is derived from review requests, the latest review and the version after it. Per-user pin and timed snooze. ASC's plugins call this API directly. |
| Actions | Context gains `asc_change`, `asc_stack_position`, `asc_stack_size` and `asc_stack_parent_sha`. Workflows run `asc affected --base $asc_stack_parent_sha` to test only the change's own projects. |
| Undo-aware operations | "Restore version v" (moves `refs/pull/<n>/head` back to a kept version); "Revert landed stack" (`Apply(REVERT)` over the `FindLanding` results, newest first) |

---

## 21. Branchless by default and commit cloud [audit: branchless-folded]

Normative. Source: `docs/research/branchless.md` (decisions BD1-BD12), which implements the checklist decision "branchless by default" (2026-10-22, `scion-checklist.md`). This section overrides three earlier rules, by user decision: "nothing is uploaded before `asc push`" (old §6.3), "no write-behind" (old §0) and "secrets in drafts never leave the machine" (old §15.2). The compensating controls are the opt-out commit cloud (§21.3), the pre-upload secret hold (§21.3.7) and purge. It builds on §20 (`ChangeMeta`, the `changes` index, `change_versions`, the `Changes` service and D1-D12 are used and not restated). PLAN units are B01-B28 (PLAN §B).

Fixed rules this section obeys: git commands keep git's exact semantics, output and exit codes; the default branch is `master`; the IDEs' built-in git is not supported (ASC's own IDE plugins, §24, replace it); there is no LFS; memory is bounded and nothing scans the whole repo; local-only mode keeps working.

### 21.1 Decisions

| # | Decision | Why |
|---|---|---|
| BD1 | **A draft is a non-immutable changeset that has a `ChangeMeta`** (§20.3). Nothing names it except its change id and changeset id. A branch is never needed. | Scion and Sapling: the draft/public boundary decides what is work in progress, not a ref. |
| BD2 | **Visibility is a set of draft heads, not refs.** Local: `state.db.draft_heads`. Server: `cloud_heads` per user cloud workspace. Visible drafts are the draft ancestors of visible heads. | jj's view and Commit Cloud's heads. Unlike git-branchless, no keepalive refs appear in `for-each-ref`, and advertisement cost does not grow with drafts (the infinitepush lesson). |
| BD3 | **A local branch is a real local ref** (`state.db.refs`, `refs/heads/<b>`), exactly as today. In branchless mode it is a *label*: it is never a shared server ref. | Every local git command then behaves and prints exactly as git, in both modes (`On branch X`, `HEAD detached at`, `for-each-ref`, turbo's `HEAD`). |
| BD4 | **Shared refs are only the patterns in the repo's `shared_refs`** (default `refs/heads/master`, `refs/heads/release/*`, `refs/heads/green/*`, `refs/tags/*`). Every other pushed `refs/heads/<n>` becomes a **user ref** `refs/users/<owner-id>/heads/<n>` (`<owner-id>` is the numeric Forgejo user id, never the login; the login is resolved only for display and for explicit refspecs, so a rename breaks nothing): one CAS row in the existing `refs` table, advertised only to its owner (as `refs/heads/<n>`). A plain git push (`ImportGitPush`) of such a branch becomes a user ref too (§21.5.1). | Scion principle 6 holds: no global state per draft. The checklist's "user-scoped branches". Git push/fetch semantics stay intact for the owner. |
| BD5 | **`git push origin feat` in branchless mode = user-ref CAS plus submit of the stack** in one `UpdateRefs`: `refs/heads/feat` (user ref) plus one `refs/for/<trunk>/asc-<change>` per change of `trunk..feat` (§20.1 D8, C24). PRs are created as **drafts** (`asc.push.review`). Output is git's push output for `feat`. | The checklist line "pushing a branch submits its stack as draft changes". Rejections, `-u`, `--force-with-lease` and `--delete` keep git's semantics because the user ref is a real CAS row. |
| BD6 | **Commit cloud:** when online with a daemon, each new changeset and its `ChangeMeta` are uploaded asynchronously (`HasObjects`, `PutObjects`, `Ingest`), then the head set is updated with a single-row CAS (`Cloud.UpdateWorkspace`). Nothing waits for it. Upload is want-only (explicit ids, no have/want negotiation, no packs) and idempotent. **Upload is gated (§21.3.7):** never on a public repo, only when the repo's `cloud` flag is on, and only after the pre-upload ignore and secret check. | Scion: "nothing to push". `push`/`submit` then find nothing to upload. Local-only and offline work are unchanged. |
| BD7 | **No local git command changes its behaviour or output with the mode.** The compat suite runs the local commands in both modes and must produce byte-identical results; the byte-identical claim covers local commands only. `push` (and the `fetch` cloud pull) are the two commands with a remote effect that depends on the mode: what the server stores for a non-shared push, whether the push submits PRs, whether tags are accepted, and whether drafts upload in the background. Their exit codes, stderr and stdout for the ref lines stay git's. This is a recorded amendment to §20.1 D4, which is edited to say so. | §20.1 D4 for local commands; the checklist decision "Decision: branchless by default" (`scion-checklist.md`) for the remote effect of `push`. |
| BD8 | **Hidden is never deleted at once.** `asc hide` (and rewrites, `asc abandon`, cloud removal) drop heads; objects stay addressable by id. Server: hidden heads stay GC roots for 30 days; visible heads untouched for 180 days with no open PR are auto-hidden. Local: hidden drafts have their own retention, `asc.draft.hiddenDays` (default 90), independent of the op-log cap of §20.1 D6 (1,000 ops or 90 days, whichever comes first). | Sapling hide/unhide, jj's 2-week GC, Commit Cloud's age-based omission. Recovery is always `asc unhide <id>` within the grace period. |
| BD9 | **Scope:** drafts are owned by the authenticated Forgejo user (stored by id). Objects are readable by id by anyone with repo read (the namespace rule, §15.1, unchanged), which is why upload is off on public repos and guarded by a secret check (§21.3.7). Listing another user's cloud workspace needs repo write. Only the owner (or an admin, for hide/delete) changes a workspace or a user ref. The owner may purge their own unsubmitted drafts; an admin may purge any. | Scion's "available everywhere" for content, while the listing of work in progress stays among collaborators. This reverses §15.2 "secrets in drafts never leave the machine"; §21.3.7 is the compensating control. |
| BD10 | **Two switches.** Client `asc.branchless` (bool, default `true`; system/global/local/worktree/`-c`/env) for client behaviour, and the server repo policy `repos.branchless` (default `true` for new repos and after migration) for ref policy. Per-user opt-out is the client key; per-repo opt-out is the server setting. | The checklist: "switch it off per repo or per user". The server decides what a ref is; a client cannot create shared refs by configuration. Cloud upload needs the server's `repos.branchless` and `repos.cloud` (§21.6.1); a client key alone never uploads to a repo that has it off. |
| BD11 | **Actions and webhooks fire only for shared refs and PRs.** User refs emit no Forgejo push event and no branch row. CI for drafts runs on `pull_request` events of the per-change PRs (whether Forgejo Actions fires them for draft PRs is unverified; spike S13 (§19), run by B27, §21.4.4). | No global fan-out per draft. ASC's workflows already use `pull_request` plus `push: [master]`. |
| BD12 | **Branchless server policy needs an asc-primary repo (Stage ≥ 2).** Stage 0 (git remote) and Stage 1 (mirror) are branch mode on the server; the client keeps local draft heads only. | Forgejo git is the truth before Stage 2, and it has branches. |

### 21.2 Model

#### 21.2.1 Terms

| Term | Definition | Cost |
|---|---|---|
| draft | §20.3 "draft change": not reachable from a trunk or `asc.stack.immutable` ref, has a `ChangeMeta` | `is_ancestor` via skip lists |
| draft head | A row in `draft_heads` (local) or `cloud_heads` (server) | O(1) |
| visible draft | A draft that is an ancestor of a visible head, found by a first-parent walk from the head that stops at the first immutable changeset (C05) | O(heads × stack length × log n) |
| hidden | A head with `hidden_at` set. Its drafts are invisible unless another visible head reaches them. | O(1) |
| obsolete | §20.3: a visible successor names it in `preds` | local index |
| cloud workspace | `(repo, owner, name)`, default name `default`; optional per-host name | one row |
| user ref | `refs/users/<owner-id>/heads/<n>` in the server `refs` table | one row, CAS |

#### 21.2.2 Local state (`state.db`, additive)

```sql
draft_heads(repo_id, cs BLOB, change BLOB, added INT, hidden_at INT NULL, PRIMARY KEY(repo_id, cs))
  + INDEX (repo_id, hidden_at)
cloud_queue(repo_id, cs BLOB, enq INT, attempts INT, next_at INT,
            state INT /*0 pending, 1 failed, 2 too_large*/, last_error TEXT, PRIMARY KEY(repo_id, cs))
cloud_state(repo_id PRIMARY KEY, ws_name TEXT, version INT, last_sync INT)
cloud_base(repo_id, cs BLOB, PRIMARY KEY(repo_id, cs))      -- head set at last successful sync (3-way base)
```

Maintenance runs in the same transaction as the ref and op-log write (C04 sinks), so it costs a few rows per command:

| Event | `draft_heads` | `cloud_queue` |
|---|---|---|
| new changeset X (commit, merge, cherry-pick, `asc create`) | insert X; delete each parent of X that is a head | upsert X; delete queued parents not yet started (coalesce: uploading X uploads them) |
| rewrite old → new (amend, rebase, restack, absorb, …) | delete `old` if it is a head; insert `new` (with descendants' new ids) | as for a new changeset |
| commit becomes immutable (fetch moves a trunk over it, land) | delete heads that are now immutable | delete queued entries that are now immutable |
| `asc hide <rev>` | set `hidden_at` on every head that has `<rev>` as an ancestor; insert `<rev>`'s parent as a head when it is a draft with no other visible child | remove those heads from the queue |
| `asc unhide <id>` | clear `hidden_at`, or insert the id as a head | enqueue |

Maintained in both modes (it is invisible). Only branchless mode reads it for `asc sl`, upload and GC.

#### 21.2.3 Server state (additive)

```sql
cloud_ws(ns BLOB, owner BIGINT, name TEXT, version BIGINT, updated BIGINT, PRIMARY KEY(ns, owner, name))
cloud_heads(ns BLOB, owner BIGINT /*Forgejo user id*/, name TEXT, cs BLOB, change BLOB /*derived by the server from m/, never client-supplied*/, labels TEXT /*json, validated, display only*/,
            added BIGINT, hidden_at BIGINT NULL, PRIMARY KEY(ns, owner, name, cs))
  + INDEX cloud_heads_hidden (ns, hidden_at), INDEX cloud_heads_added (ns, added)
repos += branchless BOOL, cloud BOOL /*default = branchless; owners can switch cloud off alone*/, shared_refs TEXT /*json globs*/, legacy_shared TEXT /*json names, migration only*/
user_ref_meta(ns BLOB, owner BIGINT, name TEXT, landed_at BIGINT NULL, PRIMARY KEY(ns, owner, name)) + INDEX (ns, landed_at)
cloud_usage(ns BLOB, owner BIGINT, bytes BIGINT, workspaces INT, PRIMARY KEY(ns, owner))   -- quota accounting (§21.3.7)
-- user refs: rows in the existing refs/ref_log tables (§3.4), name = refs/users/<owner-id>/heads/<n>; user_ref_meta holds only the landed marker
```

The draft index by change id is the existing `changes` table and `changes_by_change` index (§20.4): `Ingest` already writes a row for every changeset whose closure carries a valid `m/` object. `cloud_heads` adds only the visible set. Stack shape stays derived from the graph (§20.1 D3).

#### 21.2.4 GC roots and expiry

| Root or rule | Local | Server |
|---|---|---|
| Visible heads | `draft_heads` with `hidden_at IS NULL`: their objects stay in `store/` until uploaded, then demote to `cache/` (as after push, §6.3 step 5) | every `cloud_heads.cs` (visible or hidden within grace) is a GC root (amends §3.6) |
| Hidden heads | kept for `asc.draft.hiddenDays` (90) after `hidden_at`, independent of the op log; then `asc gc` deletes un-uploaded objects only they reach. `asc undo` of an op whose head expired fails with "objects expired" and changes nothing | rows with `hidden_at < now − hidden_grace_days` (30) are deleted by the expiry worker; the objects become collectable by O01's two-mark sweep |
| Stale visible heads | none (local work is never auto-hidden) | `added < now − abandon_days` (180) and no open PR for any change in the stack → `hidden_at = now` |
| Submitted versions | – | `change_versions` (§20.4), unchanged |
| User refs | – | ordinary refs (roots); `agent/*` user refs keep their TTL (§8.8); `user_ref_meta.landed_at` is set in the land transaction (§21.2.4a) and the ref is deleted `landed_ref_days` (7, 0 disables) later |
| Pull window | – | heads older than `asc.cloud.maxSyncAge` (14 days) are not pulled to new machines; `asc cloud list --all` shows them |
| Secret purge | – | `asc cloud purge <change>` (owner for their own unsubmitted drafts, repo admin for any): deletes the heads, lists the objects in `gc_purge` and queues `cache_evict` for every cache tier (§21.3.7); O01 sweeps a purged object at its next run if no ref or head reaches it, without the 14-day age rule |

The worker runs hourly in batches of 1,000 rows using the indexes `cloud_heads_hidden`, `cloud_heads_added` and `user_ref_meta (ns, landed_at)`; it never scans `changes` or `refs`.

**1.4a Bounded lookups.**
- *Landed marker.* When a land (`UpdateRefs(LAND)`) succeeds, the same transaction looks at the user refs of the landing author only (at most `max_user_refs`, 1,000 per user) and, for each, checks the change ids of `trunk..tip` (at most the stack-length cap of §20.3) against `land_map`. If all landed, it sets `landed_at`. Nothing walks other users' refs or the whole `refs` table.
- *`push --delete` reachability.* For each change in the deleted ref's range (stack-length cap), the server asks whether it is reachable from the owner's other heads: the owner's `cloud_heads` (at most 10,000) and the owner's other user refs (at most 1,000), by `is_ancestor` with skip lists. The set is capped at 11,000 tips; if the cap is exceeded the PR is left open and the `remote:` line says so. It never reads another owner's heads.

#### 21.2.5 Scoping and access

| Action | Owner | Repo write | Repo read | Repo admin |
|---|---|---|---|---|
| Read draft objects by id (`GetObjects`, `ResolveChanges`, `Stack`, `asc stack get <change>`) | yes | yes | yes | yes |
| List a cloud workspace (`Cloud.GetWorkspace`, `asc sl --cloud --user u`) | yes | yes | no | yes |
| Update a workspace (add, remove, hide) | yes | no | no | hide only |
| Push or delete a user ref | yes | no | no | delete only |
| Fetch another user's ref by explicit refspec `refs/users/<login>/heads/<n>` (the login is resolved to the id on each request) | yes | yes | no | yes |
| Purge a draft (`Cloud.Purge`) | own unsubmitted drafts | no | no | any |
| Receive `Watch KIND_CLOUD` events | own workspaces only | no | no | no |

### 21.3 Commit cloud

#### 21.3.1 Upload pipeline (daemon)

1. Take the oldest `pending` row of `cloud_queue` whose `next_at ≤ now`. Run the guard of §21.3.7 on its closure first (ignored paths, denied paths, secret scan); a held changeset is not uploaded.
2. Closure: the new changesets from the head back to the first one the server knows (the remote-tracking tips and the last synced heads bound the walk), their new trees by spine, files, chunks and `m/` objects. This is push step 1 (§6.3) with a 256-id batch window.
3. `HasObjects` in batches of 4,096, then `PutObjects` of the missing ids only, files and chunks first, trees bottom-up, changesets last. Objects are stored bytes; puts of content-addressed objects are idempotent.
4. `Ingest` the changesets (§8.7). It writes `cs_graph`, `git_map` and `changes` rows, with no ref.
5. Sync the head set (§21.3.4) with one `Cloud.UpdateWorkspace` CAS carrying an `op_id` (idempotent through `op_results`).
6. Delete the queue row; demote the uploaded objects from `store/` to `cache/`.

`push`, `submit` and `asc cloud sync` drain the queue entries in their closure first; because every step is idempotent, a race with the daemon costs only a repeated `HasObjects`.

#### 21.3.2 Bounds

| Bound | Value | Behaviour at the bound |
|---|---|---|
| Queue length | ≤ visible heads (coalesced, §21.2.2) | `asc.cloud.queueMax` 10,000 heads; beyond it new heads stay local and `asc cloud status` reports `queue full` |
| Changeset size | `asc.cloud.maxChangesetSize` 1 GiB of new content | the entry is marked `too_large`; `push` still uploads it |
| Concurrency | `asc.cloud.streams` 2 `PutObjects` streams, ≤ 16 MiB batches | – |
| Daemon memory | the batch window only; closure walked spine by spine | within the 64 MiB idle and bounded-load budget (§16.4) |
| Server | `PutObjects` in class `objects`; `Ingest` and `UpdateWorkspace` in a new class `background` that is shed first | `RESOURCE_EXHAUSTED` with `retry-after-ms` |
| Server heads | `[cloud] max_heads_per_workspace` 10,000 | `CLOUD_HEADS_LIMIT`; the user hides heads |
| Server workspaces | `[cloud] max_workspaces_per_user` 16 per repo; workspace names match `[A-Za-z0-9._-]{1,64}` | `CLOUD_WORKSPACES_LIMIT` / `INVALID_ARGUMENT` |
| Server user refs | `[cloud] max_user_refs` 1,000 per user per repo | `USER_REFS_LIMIT`; the user deletes refs |
| Server bytes | `[cloud] max_user_bytes` per user per repo, charged at `PutObjects`/`Ingest` (§21.3.7) | `QUOTA_EXCEEDED` |

#### 21.3.3 Retry, offline and modes

| Situation | Behaviour |
|---|---|
| Transient error (`UNAVAILABLE`, `RESOURCE_EXHAUSTED`, `DEADLINE_EXCEEDED`) | backoff with full jitter from 1 s to 5 min, unbounded attempts, honouring `retry-after-ms` |
| Permanent error (`INVALID_OBJECT`, `PERMISSION_DENIED`, `PATH_POLICY`) | `state = failed`, `last_error` kept, retried only when the head changes or on `asc cloud sync` |
| Offline (connect fails) | the queue pauses; the daemon probes `Ping` with the same backoff; commands never block or print anything |
| No daemon (`ASC_NO_DAEMON`, CI jobs, tests) | nothing uploads in the background; `push`, `submit` and `asc cloud sync` upload in-process |
| local-only, bucket remote, git remote, mirror-mode server, server without the `cloud.v1` feature, a repo with `branchless` or `cloud` off, or a public repo | cloud is off; draft heads stay local |
| `asc.cloud=off` or `asc.branchless=false` | cloud is off |
| Held by the guard (§21.3.7) | `state = held`; `asc cloud status` shows the reason; `asc cloud release <cs>` uploads it anyway |

#### 21.3.4 Multi-machine sync (Commit Cloud algorithm)

1. Upload local queued heads (§21.3.1 steps 1–4).
2. `GetWorkspace(since = cloud_state.version, max_age = maxSyncAge)` returns the remote heads.
3. Three-way merge against `cloud_base`: union of heads; a head removed on one side since the base is removed on the other (locally it gets `hidden_at`, so it is recoverable); a remote head that has a local head in its `preds` closure replaces it.
4. `UpdateWorkspace(base_version, add, remove)`. On `VERSION_CONFLICT`, repeat from step 2, at most 3 times, then report it in `asc cloud status`; the next tick retries.
5. Pull the metadata of new remote heads (`Changes.Stack`, `LogEntry`s only, stopping at immutable changesets). Files stay lazy.

Rules: sync never moves `HEAD`, a branch or the working copy. A checked-out change that became obsolete is shown by `asc sl`, and the user moves with `asc next` or `asc sync`. Labels are synced as display text only and never become `refs/heads/*` on another machine (git's branches are local). The daemon syncs every 5 minutes, on `Watch` events of kind `KIND_CLOUD` (sent to the workspace owner only), and when `git fetch` or `asc sl` asks it to. That ask is a non-blocking request to the daemon; with no daemon nothing happens (§21.4.2). `asc sl --cloud` and `asc cloud sync` run the sync in-process and report failures, because they are asc-only verbs.

#### 21.3.5 RPCs (`proto/asc/v1/cloud.proto`, same port)

```proto
service Cloud {
  rpc GetWorkspace(GetWorkspaceRequest) returns (GetWorkspaceResponse);          // cheap
  rpc UpdateWorkspace(UpdateWorkspaceRequest) returns (UpdateWorkspaceResponse); // background, x-asc-op-id
  rpc ListWorkspaces(ListWorkspacesRequest) returns (ListWorkspacesResponse);    // cheap, paged
  rpc Purge(PurgeRequest) returns (PurgeResponse);                               // admin only
}
message CloudHead { bytes cs = 1; bytes change = 2; /* response only; the server derives it from m/ and ignores the client value */
  repeated string labels = 3; /* <= 8, each <= 64 bytes, [A-Za-z0-9._/-], else INVALID_ARGUMENT */ int64 added = 4; int64 hidden_at = 5; }
message GetWorkspaceRequest { RepoRef repo = 1; string owner = 2; /* empty = caller */ string name = 3;
  uint64 since_version = 4; int64 max_age_s = 5; bool include_hidden = 6; string page_token = 7; }
message GetWorkspaceResponse { uint64 version = 1; repeated CloudHead heads = 2; repeated bytes removed = 3;
  uint32 omitted = 4; string next_page_token = 5; }
message UpdateWorkspaceRequest { RepoRef repo = 1; string name = 2; uint64 base_version = 3;
  repeated CloudHead add = 4; repeated bytes remove = 5; repeated bytes hide = 6; repeated bytes unhide = 7; }
message UpdateWorkspaceResponse { uint64 version = 1; }   // ErrorDetail.code VERSION_CONFLICT carries the current version
// repo.proto, additive: Version.features += "cloud.v1", "branchless.v1"; ErrorDetail.code += VERSION_CONFLICT,
//   CLOUD_HEADS_LIMIT, CLOUD_WORKSPACES_LIMIT, USER_REFS_LIMIT, QUOTA_EXCEEDED, REF_NOT_SHARED; RepoConfigResponse += branchless, cloud, shared_refs, visibility; WatchEvent kind += KIND_CLOUD.
```

#### 21.3.6 Budgets (benchmark gate, O05)

| Metric | Target |
|---|---|
| `commit` latency with the daemon uploading | unchanged within noise (the enqueue is one row in the existing transaction) |
| Time from commit to visible on the server, ≤ 10 changed files, online | p50 < 2 s, p95 < 10 s |
| `push` of an already uploaded stack | one `HasObjects` batch plus one `UpdateRefs` |
| `UpdateWorkspace` | O(changed heads), one row CAS plus head rows |
| `ListRefs` for one user | O(shared refs + own user refs); independent of the number of drafts and users |

#### 21.3.7 Secrets, quota and untrusted fields

Draft objects are readable by every repo reader, including CI tokens and cache tiers, so cloud upload is guarded. This replaces §15.2 "secrets in drafts never leave the machine" with the controls below.

| Control | Rule |
|---|---|
| Public repos | No auto-upload. `RepoConfig.visibility = public` turns the repo's `cloud` flag off. `asc.cloud=on` on a public repo is refused with a message; the user must use `asc push` / `asc submit`. |
| Pre-upload guard | In the client, before step 2 of §21.3.1: (a) paths ignored by `.gitignore`/`.ascignore` and the default deny list `asc.cloud.denyPaths` (`.env*`, `*.pem`, `*.key`, `id_*`, `*credentials*`) are held; (b) a bounded secret scan of added blobs of at most 1 MiB each (known token patterns and high-entropy assignments) holds the changeset. Held changesets stay local until `asc cloud release`. `push`/`submit` apply the same guard unless `--no-secret-check` is given. |
| Owner purge | `Cloud.Purge` by the owner works on drafts that have no PR, no `change_versions` row and no shared-ref ancestry. An admin can purge any. |
| Cache eviction | `Purge` writes `gc_purge` and, in the same transaction, `cache_evict(ids)` outbox rows. The outbox worker (U43c) sends the ids to every configured cache tier (CI host cache, `cacheproxy`, bucket/CDN cache) until each acknowledges; `asc cloud purge` reports the unacked tiers. Copies already fetched to client machines cannot be recalled; the command says so. |
| Quota | `PutObjects` and `Ingest` charge bytes to the user's `cloud_usage` and to the Forgejo quota API, because user-ref pushes and cloud ingest skip pre-receive. User-ref updates through `UpdateRefs`/`ImportGitPush` run the same check. Workspaces per user and user refs per user are capped (§21.3.2). |
| Untrusted fields | `CloudHead.change` is derived by the server from the `m/` object of `cs`. `labels` are validated (§21.3.5). Forgejo B23 and the MCP `drafts` tool render labels and messages as escaped text, and MCP marks them untrusted data (mcp.md "Untrusted content"). `Watch KIND_CLOUD` goes to the owner only. |

### 21.4 Git DX mapping

#### 21.4.1 What a branch is

- **Local:** a real ref in `state.db.refs`, as today. `HEAD` in the synthetic gitdir is `ref: refs/heads/<b>` on a branch and a SHA when detached (§9.4). asc never invents or hides a branch.
- **Status line:** `On branch <b>` when `HEAD` is symbolic, `HEAD detached at <sha>` / `HEAD detached from <sha>` when detached, exactly as git. An anonymous draft is a detached `HEAD`.
- **Leaving commits behind:** `checkout`/`switch` away from detached drafts prints git's "Warning: you are leaving N commits behind…" byte for byte. The commits stay visible through `draft_heads`, so `asc sl` still shows them; the warning is not changed (BD7).
- **Server:** in a branchless repo a pushed non-shared branch is the pusher's user ref (BD4), so `origin/<b>` exists for its owner only.

#### 21.4.2 Command table

"Same" means git's semantics, output and exit code, with the refs that exist.

| Command | Branchless mode | Branch mode |
|---|---|---|
| `branch`, `branch -d/-D/-m/-c/-u`, `branch -r/-a/-vv` | Same (local refs). `-r` lists shared refs plus the caller's own user refs. | Same; `-r` lists every server branch |
| `checkout -b`, `switch -c/-C`, `checkout <b>` (DWIM from `origin/<b>`) | Same. DWIM works for the owner's pushed branches. | Same |
| `checkout <sha>`, `switch --detach` | Same; commits made there become draft heads | Same; commits become draft heads (unused) |
| `status` | Same (`On branch` / `HEAD detached`, ahead/behind against `origin/<b>`). Cloud upload never changes ahead/behind. | Same |
| `commit`, `merge`, `rebase`, `cherry-pick`, `reset`, `stash` | Same; draft heads and the upload queue are updated invisibly | Same; draft heads only |
| `tag` (local) | Same | Same |
| `push origin feat` (non-shared name) | User-ref CAS plus submit of `trunk..feat` (§21.4.3); remote effect per the amended §20.1 D4 | Creates or updates the server branch (git) |
| `push origin master` / `release/x` / tags in `shared_refs` | Same: shared-ref CAS, branch protection, `--land`/`-o land` | Same |
| `push origin <tag>` not in `shared_refs` | `! [remote rejected] <tag> -> <tag> (asc: ref not shared in this repository)`, exit 1 (a remote-effect difference, covered by the amended §20.1 D4; `asc.branchless=false` or `repos.branchless=false` restores git) | Same as git |
| `push --delete feat` | Deletes the user ref; closes the open PRs of changes that no other user ref or cloud head of the owner reaches | Deletes the server branch |
| `push --force`, `--force-with-lease` | CAS on the user ref with git's rules; the PRs get new versions (lease of §20.5) | Same as git |
| `push -u`, `push` with `push.default` | Same (upstream `origin/feat`) | Same |
| `push --all` | User refs only: each non-shared local branch becomes a user ref and shared-pattern branches follow their CAS rules, but no `refs/for` submit and no PR is created (ASC has about 350 local branches). The output ends with `remote: asc: --all does not submit stacks; use asc submit`. | Same as git |
| `push --mirror` | Rejected before any network call: `fatal: asc: --mirror is not supported on a branchless repository (it would delete refs you cannot see); use --all and --delete`, exit 128 | Same as git |
| `fetch`, `fetch --prune` | Same over the advertised set (shared + own user refs + `refs/pull/*` if configured). The cloud pull is best-effort and runs only in the daemon: `fetch` sends the daemon a non-blocking sync request after its own work. A failed pull or a missing daemon (`ASC_NO_DAEMON`, CI, tests) never changes `fetch`'s exit code, stdout or stderr; the failure appears in `asc cloud status` only. | Same over all server branches |
| `pull` | Same (fetch, then merge or rebase per config) | Same |
| `ls-remote` | Same over the advertised set | Same |
| `log --branches`, `log --all`, `for-each-ref`, `show-ref` | Same: refs only. Anonymous drafts are not refs and do not appear (no `refs/branchless/*`-style keepalives) | Same |
| `rev-parse`, `rev-parse --abbrev-ref HEAD`, `symbolic-ref` | Same | Same |
| `worktree add -b agent/<n>` (Claude Code hooks) | Same; `push -o asc.ttl=7d` makes an expiring user ref | Same; expiring shared branch |
| `asc create` | No label unless a name is given; on a branch the branch advances as with `commit` | Names a branch from `asc.stack.branchTemplate` (C11) |
| `asc submit` | As C24; also updates the user ref of the current branch if it has one | As C24 |
| `asc sl` | Heads = local branches + workspace `HEAD`s + visible draft heads; `--cloud` adds other machines' heads | Heads per §20.3 |

#### 21.4.3 `git push origin feat` in branchless mode

1. Resolve the stack trunk of `feat` (C05). Collect `trunk..feat` first-parent. Commits without a `ChangeMeta` get an id by the C01 import rule (jj header, then Gerrit trailer, then the migration-mode derived id, §21.6.2).
2. Upload the closure (§21.3.1; usually already done).
3. One `UpdateRefs` (`--atomic` semantics): `refs/heads/feat` (the server maps it to the user ref; expect = the `origin/feat` value or the lease) plus `refs/for/<trunk>/asc-<change>` per change, bottom to top, with push option `asc.draft` per `asc.push.review`. If the range contains a merge changeset, the push is rejected before any update (§20.3 ends a stack at a merge, and D1 allows one change per review): `! [remote rejected] feat -> feat (asc: stack contains a merge commit; rebase it or submit changes individually)`, exit 1, nothing moved. A rebase that removes the merge, or `asc.push.review=none` (user ref only), avoids the rejection.
4. Any per-change rejection (for example `not allowed to update change <short>`) fails the whole update: `! [remote rejected] feat -> feat (<reason>)`, exit 1, nothing moved.
5. Output is git's for the `feat` line only, plus the server's `remote:` lines (PR URLs), which git also prints verbatim:

```
remote: asc: 3 changes: 2 new draft PRs, 1 updated
remote:   https://forge.example.org/asc/monorepo/pulls/412
To https://forge.example.org/asc/monorepo.git
 * [new branch]      feat -> feat
```

`asc.push.review = none` sends only the user ref; `publish` creates ready PRs. Here `<trunk>` is the default branch (`master` for ASC). The compat suite strips `remote:` lines, as it already does for server-specific text.

#### 21.4.4 Turbo and CI

| Caller | Effect |
|---|---|
| turbo (`.git/HEAD`, `.git/index`, `rev-parse`, `ls-files`, `diff-tree --merge-base`, `status`) | none: all local, branches are real refs |
| `TURBO_SCM_BASE`/`HEAD` in Actions | For a stacked PR `GITHUB_BASE_REF` is the trunk (D8: `base_branch` = trunk), so turbo would diff against `<default_branch>` and include the parent changes. `asc ci checkout` therefore exports `TURBO_SCM_BASE` = the bridge SHA of the PR's `merge_base` (the parent change) and `TURBO_SCM_HEAD=HEAD`, as turbo-ci.md already does when a base is set; turbo's explicit base wins over `GITHUB_BASE_REF`. A workflow that skips `asc ci checkout` gets the `<default_branch>` diff, a safe superset. |
| `asc ci checkout` | checks out `refs/pull/<n>/head`, a changeset id or a shared ref; it never needs a user ref. CI has no daemon, so it uploads nothing. It always fetches `<default_branch>` and writes `origin/<default_branch>` (not only when a base is set), so turbo's `origin/<ref>` fallback (`github_actions_remote_base_ref_fallback`, turbo-ci.md) resolves when `TURBO_SCM_BASE` is unset |
| Workflows `on: pull_request` | fire for every per-change PR. **Unverified for draft PRs:** whether Forgejo Actions fires `pull_request` for a draft PR is not documented in the sources read. Spike S13 (§19) tests it on the fork, as unit B27. If it does not fire, B17 patches the fork to fire it, or `asc.push.review` defaults to `publish` on repos that need CI on every push |
| Workflows `on: push: branches: [master]`, `release/*`, `green/*` | unchanged |
| Workflows `on: push` for other branches | no longer fire in a branchless repo; `asc doctor --workflows` lists them (B24) |

### 21.5 Server

#### 21.5.1 Ref policy (applied in `UpdateRefs` step 1 and in `ListRefs`)

| Ref pushed by a client | Branchless repo | Branch-mode repo |
|---|---|---|
| matches `shared_refs` | shared ref: CAS, pre-receive (branch protection), outbox `post_receive`, bridge | same |
| other `refs/heads/<n>` (asc push or `UpdateRefs`) | user ref `refs/users/<owner-id>/heads/<n>`: CAS, owner-only, no pre-receive, no `post_receive`, no bridge, TTL allowed for `agent/*` | shared branch |
| other `refs/tags/*` | rejected `REF_NOT_SHARED` | shared |
| `refs/for/*` | AGit (§13.5, C34) | same |
| `refs/pull/*`, `refs/snapshots/*` | server-managed, as today | same |
| plain git push through `ImportGitPush` to a non-shared branch | accepted as the pusher's user ref `refs/users/<owner-id>/heads/<n>` (same CAS, same output as git; the imported commits get ids by the C01 rule). No PR is created, because a plain push carries no `refs/for` ref; the pusher runs `asc submit` or pushes with `asc`. AGit pushes (`refs/for/<default_branch>`) work as before. | shared branch |

`shared_refs` defaults to `[refs/heads/<default_branch>, refs/heads/release/*, refs/heads/green/*, refs/tags/*]` plus `land_branches`; it is set per repo through `RepoAdmin` and the Forgejo repo settings (B17). `ListRefs` returns shared refs plus the caller's own user refs renamed to `refs/heads/<n>`; an explicit `refs/users/<login>/` prefix lists another user's (write permission; the login is resolved to the id per request). A user ref and a shared ref can never collide, because the name decides the class.

#### 21.5.2 Forgejo

| Concern | Branchless behaviour |
|---|---|
| PRs | One AGit PR per change, `base_branch` = trunk, `merge_base` = parent commit (§20.1 D8, C34). No head branch exists. Push creates them as drafts by default. |
| Branch-chain PRs (C46) | Branch mode only |
| Branch page and branch table | shared refs only; user refs never create rows |
| Webhooks | `push` for shared refs; `pull_request` and review events per change PR; none for user refs or cloud heads |
| Actions | §21.4.4; the stack context of C40 is unchanged |
| Drafts page | "Your drafts" (v1.x, B23), rendered from `Cloud.GetWorkspace` |
| Repo settings | "Branchless" toggle and the `shared_refs` list, stored in asc through `RepoAdmin` (B17) |

#### 21.5.3 Stages

| Stage | Server ref policy | Cloud |
|---|---|---|
| 0 git remote, 1 mirror | branch mode (Forgejo git is the truth) | off |
| 2–3 (asc truth, bridge) | branchless allowed; the bridge carries shared refs and `refs/pull/*` only | on |
| ≥ 4 | branchless, the default | on |

### 21.6 Config and migration

#### 21.6.1 Keys

| Key | Scope | Default | Meaning |
|---|---|---|---|
| `asc.branchless` | system, global, local, worktree, `-c`, env `ASC_BRANCHLESS` | `true` | client behaviour: cloud upload, `asc create` without a label, push submits the stack. It never changes local git output. |
| `asc.cloud` | same | `auto` | `auto` = on when `asc.branchless`, a server remote advertises `cloud.v1` with the repo's `cloud` flag on, and a daemon runs; `on` (still refused on a public repo); `off` |
| `asc.cloud.workspace` | global, local | `default` | `default`, `host` (the hostname) or a name |
| `asc.cloud.maxSyncAge` | global | `14d` | §21.3.4 |
| `asc.cloud.queueMax`, `asc.cloud.streams`, `asc.cloud.maxChangesetSize` | global | 10000, 2, 1 GiB | §21.3.2 |
| `asc.push.review` | global, local | `draft` | `draft`, `publish`, `none` (§21.4.3); remote effect of `push` per the amended §20.1 D4 |
| server `repos.branchless` | per repo | `true` for new repos | ref policy (§21.5.1) |
| server `repos.shared_refs` | per repo | §21.5.1 | globs |
| server `repos.cloud` | per repo | = `repos.branchless`; always off on public repos | whether the server accepts cloud upload (§21.3.7) |
| `asc.draft.hiddenDays` | global | `90` | local retention of hidden drafts (§21.2.4) |
| `asc.cloud.denyPaths` | global | §21.3.7 | paths the guard holds |
| `asc-serve.toml [cloud]` | server | `hidden_grace_days = 30`, `abandon_days = 180`, `landed_ref_days = 7`, `max_heads_per_workspace = 10000`, `max_workspaces_per_user = 16`, `max_user_refs = 1000`, `max_user_bytes`, `list_others = "write"` | §21.2.4, §21.2.5, §21.3.7 |

Effective client mode = `asc.branchless` AND the repo's `branchless` (from `RepoConfig`). Cloud upload needs the client key (`asc.cloud` not `off`), the server's `repos.branchless` and `repos.cloud`, and a non-public repo. A branchless client on a branch-mode repo therefore keeps local draft heads only and uploads nothing; push follows the server (shared branch). A branch-mode client on a branchless repo pushes user refs without submitting PRs.

#### 21.6.2 Migration of monorepo

ASC has about 117 server branches and 350 local branches (`asc-profile.md` §3.4; median 0 commits ahead of master, 79 `worktree-agent-*`).

**Server:** `asc serve admin branchless enable <repo> [--dry-run]` runs at or after the Stage 2 flip, in batches of one SQL transaction each (`ref_log.via = branchless-migrate`), and writes a report.

| Server branch | Action |
|---|---|
| `master`, `release/*`, `green/*`, tags | shared |
| head branch of an open PR | kept shared and listed in `legacy_shared` until the PR closes; then converted to a user ref of the PR author by B26 |
| fully merged into `master` (`is_ancestor`) | deleted; the old value stays in `ref_log` (PITR, `asc reflog --remote`) |
| unmerged, no open PR | user ref of the last pusher (Forgejo `branch.pusher_id`, else the repo owner), same name |
| `agent/*`, `worktree-agent-*` | user ref of the pusher with a 7-day TTL |
| unmerged commits on converted branches without a change id | `ChangeMeta` by the C01 import rule with migration mode on: a jj `change-id` header ranks first, a Gerrit `Change-Id` trailer second (§20.2 git-import row), and only when neither exists the derived id `blake3_derive("asc v1 change import", cs)[..16]`, `op = Import`. The tier is added by the follow-up unit B18a in a new file; the built C01 code is unchanged, and the id has its own derivation domain. A branchless `push` of a plain-git commit that has no `ChangeMeta` uses the same tier. |

Deletions reach Forgejo through the outbox (`post_receive` with deletions), so its branch table matches. Owners see no change on fetch (`origin/<b>` is still advertised to them); other users' `fetch --prune` prints git's ` - [deleted]` lines.

**Client:** local branches stay as they are; upstreams `origin/<b>` keep working for their owner. `asc migrate branchless [--prune-merged] [--json]` (asc-only, undoable through the op log) reports merged and stale branches, deletes merged ones with `--prune-merged`, seeds `draft_heads` from local branch tips that are drafts, and derives change ids with the same C01 rule as the server, so both sides agree with no coordination.

### 21.7 MCP

The checklist's built-in MCP decision covers this surface too. Tools are rows in `crates/asc-mcp/tools.toml` (§22.7.2, unit M03), reuse the `--json` shapes and RPC messages, are ACL-scoped and obey M-D4 and M-D9 (§22.1). Branchless adds no new toolset.

| Tool | Toolset (M-D9) | Shape | Mutating | Gate (M-D4) |
|---|---|---|---|---|
| `drafts` | `stack` (opt-in, read) | `asc sl --json` with `--cloud`; labels and messages are returned as untrusted data | no | none |
| `cloud_status` | `stack` | `asc cloud status --json` | no | none |
| `hide`, `unhide` | `write` | mutating-verb JSON (§20.7), undoable | yes | the same per-workspace rule as `create` and `modify`: `write=local` while the workspace does not upload (local-only, offline, or branchless upload off); `write=remote` with `openWorldHint: true` when the client is online in branchless mode with cloud on, because the daemon then updates the cloud head set |

Tool names are short and unprefixed, as the MCP catalogue requires (hosts namespace them by server). There is no separate push tool: `submit` (always `write=remote`) is the mapping of `asc submit` and of a branchless `git push` of a stack. The default `core` set (11 read tools) is unchanged. `hide` and `unhide` run as child verbs through the local write path of the MCP server. Purge is not exposed over MCP.

---

## 22. MCP: the agent interface, built in [audit: mcp-built-in-missing, checklist-mcp-not-folded]

Normative from v1.6 (checklist decision "MCP built in", 2026-10-22). This section folds `docs/research/mcp.md` (final, reviewed): its §N is §22.(N+1) here, its decisions M-D1–M-D10 are §22.1, and its units are PLAN §M. The research file stays as background and review history; where it and this section differ, this section wins. Related text: §21.7 (the branchless tools `drafts`, `cloud_status`, `hide`, `unhide`, which join the `stack` and `write` toolsets), §7 (`asc api` and the JSON profile), §15.1 (identity), §16.3 (rate classes). The code-intelligence `search` tool is row `search` of §22.3.2; its extra fields are specified in §23.4.4.

Fixed rules this section obeys:
- git commands keep git's semantics, output and exit codes; MCP is a new surface and changes none of them;
- the default branch is `master`;
- there is no IDE git plumbing and no LFS;
- efficiency is core: bounded memory, no whole-repo scans, everything served from derived data;
- local-only mode keeps working (`asc mcp` with no server and no daemon).

### 22.1 Decisions

| # | Decision | Why |
|---|---|---|
| M-D1 | **Three surfaces with one owner per fact.** `asc mcp` (stdio) serves the workspace and the repo. `asc serve` `/mcp` (Streamable HTTP) serves the repo without a checkout. The Forgejo fork's `/api/mcp` serves social and CI state only (PRs, reviews, issues, Actions, inbox). Forgejo exposes no file, tree, commit, diff, blame, branch or tag tool. asc exposes no PR, review, issue or Actions tool. | Same split as stacks D9: asc owns forge-neutral change facts, Forgejo owns social state. No tool exists twice with two meanings. |
| M-D2 | **Proto is the single schema source.** Tool input and output schemas are JSON Schema 2020-12, generated at build time from the `asc.v1` descriptor set. CLI `--json` output and MCP use one codec, `asc_rpc::json` (§22.4.2), the versioned "asc JSON profile" (`json.profile.v1`). `asc api` keeps standard proto3 JSON, as SPEC §7 promises, and is not affected. asc-only verbs that have no RPC get messages in a new `proto/asc/v1/cli.proto`. Every `structuredContent` is a JSON object, so a bare-array CLI shape is wrapped in a message (`asc op log --json` prints `{"ops": [...]}`). The C07 shapes are reconciled with these schemas before M02 (PLAN §M). | SPEC §11.2 said before v1.6 that `--json` is "the proto3 JSON of the matching `Ws*Response`". serde plus schemars would give a second source that drifts from the RPCs. The profile changes casing, ids and enums, so it is versioned and kept off `asc api`. |
| M-D3 | **Stateless, handle-based.** There are no MCP sessions (2026-07-28). Every call names its target: `repo` (HTTP), the workspace (stdio, implicit from the launch directory, overridable with `worktree`), and revisions or change ids as ordinary arguments. Change ids and changeset ids are the handles. | Fits the spec's "explicit handles" guidance and branchless asc: a change needs no branch, and its id is stable across rewrites (SPEC §20.2). |
| M-D4 | **Read-only by default, and only the user can widen it.** Write tools are not listed unless config enables them: `asc.mcp.write = local` for verbs with no network effect, and `remote` adds the verbs that upload or contact the network (§22.3.3 defines the split). The keys `asc.mcp.write` and `asc.mcp.confirm` are honoured only from user and system config, never from repo or worktree config. Flags (including those in a committed `.mcp.json`) can only narrow access below that ceiling: `--read-only`, a lower `--write`, fewer toolsets. On HTTP, writes also need the `asc:write` scope and `[mcp] write = true`. `--read-only` always wins. | A repository must not be able to switch on writes or switch off confirmation for the agent that opens it. GitHub, Gitea and the Forgejo community servers all ship a read-only mode. Remote effects (submit, land) cannot be undone (SPEC §20.3). |
| M-D5 | **Destructive confirmation uses MRTR elicitation** (form mode) with a sealed `requestState` (rmcp `request-state`: HMAC, principal, 5 min TTL, request digest). A client without elicitation gets an `isError` result; only the user's own user or system config (`asc.mcp.confirm = never`, M-D4) skips the confirmation. Summaries are rendered from escaped, truncated fields (§22.5). | A model-controlled `confirm: true` argument is not a confirmation. |
| M-D6 | **Every result is bounded.** A token budget (`max_tokens`, default 6,000, hard cap 25,000, the Claude Code default limit; estimated at 3 bytes per token, §22.6.2) applies to every call, with structure-preserving truncation and an opaque `next_cursor`. Streams are consumed under the budget and dropped (cancelled) when it runs out. | Agents pay per token; a 400k-file repo must not reach the model. |
| M-D7 | **Reads go through the existing RPCs. Local writes run the CLI verb as a child `asc <verb> --json`.** The child is `std::env::current_exe()`, never `asc` from `PATH` (which may be the git shim). Its stdin is null and its stdout is captured (§22.5). | Reads reuse derived data with no new code path. A child process reuses the hooks, op log, ref chokepoint (SPEC §9.4.1) and locks exactly and bounds memory per write. Writes are rare, so a process spawn costs nothing that matters. Cancellation is graceful first (§22.5). |
| M-D8 | **MCP inherits the branchless setting.** No tool creates, lists or switches branches, and `create` has no `branch` argument. `create` passes no branch flag: under branchless mode (the default, per repo or per user) the CLI default is `--no-branch`, and when a team or user has switched branchless off, the CLI names a branch from `asc.stack.branchTemplate` as it does for a human. The SPEC §20.7 edit that makes `--no-branch` the default under branchless mode is made in SPEC §20.7 (unit B01). Navigation and stacks use change ids. | This follows the checklist decision "branchless by default", including its per-repo and per-user opt-out. Git branch UX stays in the CLI. The server `commit` tool cannot move branches either (§22.3.3). |
| M-D9 | **A small default toolset.** The default `core` set has 11 read tools. Other sets (`history`, `stack`, `projects`, `search`, `write`) are opt-in through `--toolsets`/`asc.mcp.toolsets`. `tools/list` order is fixed by the manifest, so prompt caches hit. | GitHub PR #1159: the default set was cut 53% by usage, with no loss of top-20 tools. |
| M-D10 | **rmcp 3.6.0 without macros.** Tools are table-driven from the generated schemas: a manual `ServerHandler`, with no `#[tool]` and no schemars. | Schemas come from proto (M-D2), and macros would derive them from Rust types instead. |

---

### 22.2 Surfaces and placement

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

### 22.3 Tool catalogue

#### 22.3.1 Conventions

- **Names** are short, without a prefix: hosts already namespace them by server (`mcp__asc__log`). The pattern is `[a-z_]+`.
- **Input** is the RPC request message, projected:
  - injected fields are removed (`repo` on stdio, `ws_id`);
  - plumbing fields are hidden (`null_terminated`, `binary`, `id_mode`, `count_only`);
  - MCP-only fields are added: `max_tokens`, `cursor`, `worktree` (stdio), and the convenience fields listed per tool.
  - `worktree` must equal (after canonicalisation) one of the paths listed by `Workspace.Worktrees` for the launch workspace. Any other value is -32602. It is a selector, not a path to open.
  
  On HTTP, `repo` (`"owner/name"`) is required and carries `x-mcp-header: "Repo"`, so gateways can route and rate-limit on `Mcp-Param-Repo`.
- **Revision arguments** accept a git revspec, a full or abbreviated git SHA, an asc hex id, a change id prefix (at least 4 letters `k`–`z`) and `asc:<snapshot>`. They resolve in SPEC §20.2 order.
- **Output** (`structuredContent`) is the response message in the asc JSON profile (§22.4.2). A streamed RPC is folded into one message (proto merge: repeated fields concatenated). Two optional top-level fields are added: `truncated` (where the message lacks one) and `next_cursor`. The result is always a JSON object: a CLI shape that is a bare array is wrapped by its `cli.proto` message. `outputSchema` is generated, and the result always conforms.
- **Text block** is the git-format rendering, not a second copy of the JSON: a patch for `diff`, `--oneline` for `log`, porcelain v2 for `status`, `path:line:text` for `grep`. `asc.mcp.textMode = json` switches it to serialized JSON for clients that ignore `structuredContent`.
- **Annotations:** `R` = `readOnlyHint: true`. For writes, `d`/`i` = `destructiveHint`/`idempotentHint` true (otherwise explicitly false). Annotations are static per tool and cannot depend on arguments, so a tool that is destructive for some arguments is marked `d`. `openWorldHint` is false on asc tools that stay inside the closed system of server, cache and daemon, and true on `submit` and `sync` (they reach the network) and on any verb §22.3.3 classes as remote. `destructiveHint: true` means "not reversible with `undo`" or "may drop work".
- **Errors:**
  - an unknown tool or invalid arguments is JSON-RPC `-32602`;
  - everything else is `isError: true` with `structuredContent.error = {code, message, paths, conflicts, retry_after_ms}`, where `code` is `ErrorDetail.code` (SPEC §7) or `NEEDS_CONFIRMATION`, `BUDGET`, `NOT_AVAILABLE`, `STALE_OP`;
  - a child verb's exit code 1 (paused on a conflict) or 2 (ambiguous, with `candidates`) is not an error: the result is `isError: false` with `OpResult.status = "paused"` (and `conflicts`) or `"ambiguous"` (and `candidates`). Exit codes 128 and 129 are `isError`.

#### 22.3.2 Read tools

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
| `grep` | core | L→S, S | `Repo.Grep` (clean files); local for dirty files (SPEC §11.2), through the path gate (§22.5) | `GrepRequest` (`rev`, `pattern`, `fixed`, `ignore_case`, `paths`, `max_results`) | `GrepResponse` | R | 100 / 1,000; scan budget 5,000 files and 64 MiB |
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
| `search` | search | L→S, S | code-intel `CodeIntel` service (`codeintel.md` §3.4): `mode: text\|regex` → `Search`, `symbol` → `Symbols`, `semantic` → `Semantic`; listed only when `Version.features` has `search.v1` | `{query, mode: text\|regex\|symbol\|semantic, rev, paths, limit}` plus the additive optional fields `change`, `stack`, `diff_only`, `op` (`def\|refs\|callers\|outline\|find`; `symbol` mode only), `k`, `context_lines`, `max_tokens`, `rerank`, `include_excluded` [audit: codeintel-fields] | `SearchResponse`, `SymbolsResponse` or `SemanticResponse`, each with `scope`, `coverage` and `est_tokens` | R | `limit` 50 / 200; `k` 10; `context_lines` 2; `max_tokens` 4,000 (still capped by `asc.mcp.maxTokens`) |

Notes:
- `read_file` with `start_line`/`end_line` streams `ReadFile` from offset 0 on the first call and stops after `end_line`, so memory is bounded by the budget, not by the file. Its `next_cursor` carries a byte offset (and the line number at it), so a continuation seeks to that offset and costs O(page), not a restream from 0 (§22.6.2). Binary content (by attributes or a NUL in the first 8 KiB) returns metadata only (`binary: true`, size, id). In L mode with no `rev`:
  - a clean tracked path is read from the object cache by `FileId`, so a virtual placeholder is never hydrated;
  - a dirty or untracked path is read from disk, only through the path gate (§22.5): canonicalised, inside the worktree root, symlinks not followed (a symlink is reported as a link, with its target text, never read through);
  - an ignored path is refused, so `.env` files are never served. The same single ignore gate applies to `grep` (dirty files), `blame` (working copy), `stage` and the `asc://ws/…/file` resource.
- `ls_tree` with `recursive: true` needs a non-root `path`. A root-recursive call returns `isError` with code `BUDGET` and suggests `grep`, `search` or a subdirectory.
- `blame` and `line_history` without a line range use 1–200.
- `search` adds no tool of its own to the code-intelligence design (`codeintel.md` §3.4; rule M-D1 holds). Its description carries the use policy: text and regex first for identifiers, strings and error messages (verified matches); `symbol` when a name is known (`refs` is name-based, so read `confidence`); `semantic` when the names are unknown or text search found nothing (ranked chunks, not every match). Result text is untrusted file content. Every result carries `scope.changeset`, so a follow-up call can pin the same revision. There is no `code_read` (use `read_file`) and no `code_index_status` (read `coverage`). Text and regex modes in `search` are the indexed counterpart of `grep`, which stays the streaming fallback.
- `diff` keeps the `changes` list (paths and statuses) up to `max_files` = 300 and cuts patch text at hunk boundaries (§22.6.2). Files beyond 300 are counted in `omitted.files`, not listed; the cursor continues with the next 300.

#### 22.3.3 Write tools (toolset `write`; listed only when enabled, M-D4)

Local writes run `asc <verb> --json` as a child process (M-D7; §22.5 covers spawning, the stdio channel and cancellation). The child gets `GIT_TERMINAL_PROMPT=0` and `GIT_EDITOR=false` (no editor; a message must be passed), and inherits the user's environment otherwise, so hooks run as they would for `git commit`. Inputs are `*Args` messages in `cli.proto` that mirror the CLI flags. Outputs are `OpResult` (the SPEC §20.7 "every mutating verb" shape).

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

- `undo` is marked destructive because annotations are static (§22.3.1). It takes the explicit `op`, so an agent can revert only the operation it just made: a later operation by the user makes `undo` fail with `STALE_OP`. `redo` is a separate, non-destructive tool for the same reason.
- A server `commit` or `rebase` with `refs/for/*` runs the SPEC §8.3 `UpdateRefs` pipeline, so protections, hooks, the Forgejo push event and Actions triggers fire exactly as for a push. An empty `update_ref` runs only pipeline step 4 (SPEC §8.7). Server `commit` writes a `ChangeMeta` for the changeset it creates (the SPEC §8.7 gap; unit M18, which M14 needs).

Not exposed in v1: `abandon`, `split`, `squash`, `fold`, `move`, `reorder`, `reword` and every git command (the agent's shell has `git`/`asc`). Landing is Forgejo's `pr_write {method: merge}`, so policy and the queue stay in one place.

#### 22.3.4 Forgejo tools (`/api/mcp`)

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

The Forgejo endpoint is read-only unless `[mcp] WRITE = true`, and the token's scopes still apply (`read:issue`, `write:repository`, and so on). Write methods are also refused (`NOT_AVAILABLE`) until the `lockdown` mode (§22.5, unit M28) is available, so a build without it cannot enable them.

**Handle fields.** `asc_change`, `head`, `merge_base`, `stack_parent` and `version` are one `AscHandles` message in `cli.proto` (M02). The asc-mcp build also writes it as a JSON Schema file, `crates/asc-mcp/schema/asc_handles.schema.json`, which M21 vendors into `routers/api/mcp/schema/` with a recorded hash; the Forgejo tools embed it in their `outputSchema`. M17 drift-checks the hash.

#### 22.3.5 Resources

The scheme is `asc://`. `resources/list` never enumerates repository content. It returns only the workspace's `status` and `smartlog` (L). Everything else comes from templates (RFC 6570, with argument completion for `rev` and `path` from `ReadTree`, limited to 50 entries).

| URI template | Content | `ttlMs` | `cacheScope` |
|---|---|---|---|
| `asc://{owner}/{name}/blob/{rev}/{+path}{?start_line,end_line}` | file text (`read_file` rules, including binary refusal) | 86,400,000 if `rev` is a full id, else 5,000 | private |
| `asc://{owner}/{name}/tree/{rev}/{+path}{?after}` | one `ls_tree` page (non-recursive) | as above | private |
| `asc://{owner}/{name}/commit/{rev}` | `show` | as above | private |
| `asc://{owner}/{name}/change/{change}` | `change_info` | 5,000 | private |
| `asc://ws/{ws}/status`, `asc://ws/{ws}/smartlog` (L) | `status` / `smartlog` | 0 | private |
| `asc://ws/{ws}/file/{+path}` (L) | working-copy file (tracked, or untracked and not ignored), through the path gate (§22.5) | 0 | private |

Local-only repos use `owner = local` and `name` = the repo directory name. Resource annotations set `audience: ["assistant"]`.

**Subscriptions (`subscriptions/listen`, L only in v1).** `resourceSubscriptions` on the `asc://ws/…` URIs are served from `Workspace.Watch`, including `KIND_STACK` (C49). Paths and HEAD events become `notifications/resources/updated`. A process has at most 2 listens with 64 URIs each. HTTP subscriptions (ref moves on the server) are v1.x. `toolsListChanged` fires when the server later advertises a feature, for example `search.v1`.

#### 22.3.6 Prompts

Each prompt is a short template that names the tools to call. None embeds repository content.

| Prompt | Arguments | Does |
|---|---|---|
| `review_change` | `change` | `change_info` → `diff` (vs the parent) → `affected` → summarise risk by `Classify` class |
| `resolve_conflicts` | – | `conflicts` → `read_file` per side → edit on disk → `stage` → `restack` |
| `prepare_submit` | `change?` | `smartlog` → restack if needed → `affected` → `submit` (confirmed) |
| `explain_lines` | `path`, `start`, `end` | `blame` → `line_history` → `show` of the origin commits |
| `affected_tests` | `base?` | `affected --target test` → the commands to run |

---

### 22.4 Schemas and codec

#### 22.4.1 Generation

- U41's `crates/asc-rpc/build.rs` writes the `FileDescriptorSet` (`tonic-prost-build` `file_descriptor_set_path`) and exports `asc_rpc::DESCRIPTOR_SET` (a recorded amendment to U41, made by M01a, PLAN §M).
- `crates/asc-mcp/build.rs` reads `tools.toml` (one row per tool: name, toolset, surfaces, RPC, request/response message, hidden fields, added fields, defaults, caps, annotations, confirm rule, description). Using `prost-reflect` (already pinned in U01b), it emits `tools_gen.rs`: a static table with each tool's inputSchema and outputSchema as `&'static str` JSON.
- Field mapping:
  - `repeated` → array;
  - `oneof` → `oneOf`;
  - enums → string enums (§22.4.2);
  - 64-bit integers → `integer`;
  - messages → `$defs` with `$ref`, one `$defs` per schema, so inputSchema stays self-contained.
- Descriptions come from proto comments (the same source that produces `docs/api.md`, SPEC §7).
- The same build writes `crates/asc-mcp/schema/asc_handles.schema.json` from `AscHandles` (shared with the Forgejo tools, §22.3.4).
- A snapshot test (insta) freezes `tools/list` for each toolset. The default set must estimate at most 6,000 tokens.

#### 22.4.2 The asc JSON profile (`asc_rpc::json`, used by `--json` and MCP)

| Proto | JSON | Driven by |
|---|---|---|
| field names | `snake_case` (proto names) | profile default (matches SPEC §20.7 shapes) |
| `bytes` id (20 or 32 bytes) | lowercase hex (40 = git SHA, 64 = asc id) | field option `(asc.v1.json) = HEX` |
| `bytes` change id | 32 letters `k`–`z` (SPEC §20.2) | `CHANGE` |
| `bytes` path, message, text | UTF-8 string; non-UTF-8 → `{"b64": "…"}` | `PATH` / `TEXT` |
| `bytes` file data | as `TEXT`, plus `binary: true` and no data when binary | `DATA` |
| enums | lowercase value without the type prefix (`DIFF_FORMAT_PATCH` → `"patch"`); input also accepts the full name | profile |
| `int64`/`uint64` | JSON number (all asc values are below 2^53) | profile |
| `Signature` | `{name, email, time, tz}` with `tz` as `+0530`, computed from the proto's `tz_min` (the SPEC §20.7 example gains `tz`) | profile |

The option lives in a new `proto/asc/v1/options.proto` (`extend google.protobuf.FieldOptions { JsonKind json = 50101; }`). The annotations on existing fields are a recorded amendment to U41's proto files. Decoding uses the same table, so tool inputs accept exactly what outputs print.

**Versioning.** The profile is named `json.profile.v1` and is advertised in `Version.features` and in `server/discover`. `asc api` is not affected and keeps standard proto3 JSON (SPEC §7). An incompatible profile change creates `v2` and `v1` stays for one major version. Before M02, the C07 `--json` types and the SPEC §20.7 shapes are reconciled to these schemas (casing, `tz`, the `op log` wrapper).

---

### 22.5 Safety

| Concern | Rule |
|---|---|
| Default | Read-only. The write toolset is absent from `tools/list` unless `asc.mcp.write` is set in user or system config (stdio) or `[mcp] write = true` plus the `asc:write` scope (HTTP). Repo and worktree config cannot set `asc.mcp.write` or `asc.mcp.confirm`, and flags (a committed `.mcp.json` can carry any) can only narrow (M-D4). `--read-only` overrides everything. The Forgejo endpoint uses `[mcp] WRITE`. |
| Path escape (stdio) | `worktree` is accepted only if it is a `Workspace.Worktrees` entry. Every path argument passes one path gate: it is canonicalised and refused if it is absolute, has `..`, a drive or UNC prefix, or resolves outside the worktree root; each component is checked without following symlinks (open with `O_NOFOLLOW`, or `FILE_FLAG_OPEN_REPARSE_POINT` on Windows), so a symlink or junction is reported as a link and never read through. Then the same ignore gate (`.gitignore`/`.ascignore`) applies to `read_file`, the `asc://ws/…/file` resource, `grep`, `blame` and `stage`. `stage --force` is not exposed. |
| Protocol channel (stdio) | fd 1 carries only JSON-RPC. At startup `asc mcp` keeps a private handle to the real stdout for the transport and redirects fd 1 (and the Windows standard output handle) to stderr, so stray prints from hooks, the engine or libraries cannot corrupt the stream. Child verbs get stdin null and stdout piped (captured as the `--json` result); their stderr is captured and logged to stderr. In-process code writes to stderr only. A test runs a hook that prints to stdout. |
| Annotations | Every tool sets all four hints explicitly (§22.3.1). Clients treat them as hints. The server enforces the same gates itself. |
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

### 22.6 Efficiency

#### 22.6.1 Every tool is served from derived data

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

#### 22.6.2 Token-aware truncation

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

- **Cursors:** a tool backed by a paged RPC reuses its `page_token`/`after`, wrapped. For other tools the cursor is an HMAC-sealed `{tool, argument digest, position}` (file index, byte offset plus line number; key handling in §22.5). A cursor from another call returns -32602.

---

### 22.7 Crate and implementation

#### 22.7.1 Dependencies

`[workspace.dependencies]` gains one line:

```toml
rmcp = { version = "3.6.0", default-features = false, features = ["server", "base64", "transport-io", "transport-async-rw", "transport-streamable-http-server", "elicitation", "request-state"] }
```

- rmcp 3.6.0 (2026-10-20, Apache-2.0, MSRV 1.88, below the workspace's 1.95) implements 2026-07-28 and keeps 2025-11-25 compatibility, so older stdio hosts that still send `initialize` work.
- Not enabled: `macros` and `schemars` (M-D10), `auth` (the server-side resource-server checks are a small axum layer of ours), and every client and reqwest feature.
- Dev-dependencies of the test units add `client` and `transport-child-process`, which give the inspector-style harness.
- M03 checks that rmcp's axum and tower match the workspace pins (`cargo tree -d` shows one axum), and that `cargo deny` passes.

#### 22.7.2 Layout and reuse

```
crates/asc-mcp            protocol layer, no repo logic
  build.rs               tools.toml + asc_rpc::DESCRIPTOR_SET → tools_gen.rs (schemas, table)
  tools.toml             the catalogue (§22.3), one row per tool
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
- **HTTP mount:** `axum::Router::nest_service("/mcp", StreamableHttpService)` in stateless mode, inside the existing `HttpRoutes` registry (U43a), behind the auth layer and the SPEC §16.3 tower limits. The PRM document is served at `/.well-known/oauth-protected-resource/mcp`.

#### 22.7.3 Auth (HTTP)

| Item | Decision |
|---|---|
| Resource server | `asc serve` is an OAuth 2.1 resource server. Requests without a token get 401 with `WWW-Authenticate: Bearer resource_metadata=".../.well-known/oauth-protected-resource/mcp", scope="asc:read"`. |
| Authorization server | Forgejo's OAuth2 provider (fork unit M26: RFC 8707 `resource`, an `aud` claim in access tokens, RFC 9207 `iss`). asc checks `aud` = its canonical `/mcp` URL through the K07 authz answer. Tokens are never passed through to other APIs. |
| Client registration | Pre-registered public client `asc-mcp` (PKCE, loopback redirects) in v1. CIMD in the Forgejo fork is v1.x (M27). DCR is not built (deprecated in 2026-07-28). |
| Forgejo PAT | Accepted as the bearer for headless agents and CI. Its Forgejo scopes map to `asc:read` (`read:repository`) and `asc:write` (`write:repository`). It has no audience, which is acceptable because Forgejo issues it for Forgejo's own resource set. |
| Scopes | `asc:read`, `asc:write`. Insufficient scope gets 403 `insufficient_scope`, so clients step up. |
| stdio | No OAuth. The CLI credentials are used (SPEC §15.1). |

---

### 22.8 Configuration

| Key | Default | Meaning |
|---|---|---|
| `asc.mcp.write` | `false` | `false` \| `local` \| `remote`. User and system config only; repo and worktree config is ignored. `remote` adds `submit`, `sync` and the verbs that upload (§22.3.3) |
| `asc.mcp.toolsets` | `core` | comma list: `core,history,stack,projects,search,write`; `all` |
| `asc.mcp.tools` | – | an explicit list; intersected with the toolsets and the write gate |
| `asc.mcp.confirm` | `destructive` | `always` \| `destructive` \| `never`. User and system config only |
| `asc.mcp.maxTokens` | `6000` | per-call budget (hard cap 25,000) |
| `asc.mcp.textMode` | `git` | `git` \| `json` |
| `asc-serve.toml [mcp]` | `enabled = false` until M13 (then `true`; `true` is refused while no auth layer exists), `write = false, allowed_origins = [], hmac_keys = [], max_tokens = 6000, max_inflight_per_principal = 8` (per instance) | the server endpoint |
| Forgejo `app.ini [mcp]` | `ENABLED = true, WRITE = false` | the forge endpoint |

CLI: `asc mcp [--read-only | --write[=local|remote]] [--toolsets <list>] [--tools <list>] [--worktree <path>]` and `asc mcp install [--scope project|user] [--no-forgejo]`. Flags override config only to narrow: they may lower `--write`, remove toolsets and set `--read-only`, never widen past the user or system ceiling (M-D4), even when a committed `.mcp.json` supplies them. `asc mcp install --scope user` therefore writes the user config keys instead of widening flags; a project-scope install writes only narrowing flags. `--read-only` overrides everything.

---

### 22.9 Rulings recorded at the fold [audit: mcp-folded]

- **Decision D12 of §20.1** said AI features stay out of core. It now says that AI review and chat stay out and that agent interfaces (MCP) are core. `docs/research/stacks.md` row 114 ("MCP server: OUT") is superseded by this section.
- **The audit's first proposal** (a tool table `MC01`–`MC03`, tools such as `search_text`, a Forgejo unit `K18`) is superseded by the reviewed design: the tools are those of §22.3, the units are M01a–M28 (PLAN §M), and `--allow-write` is `--write[=local|remote]` (§22.8).
- **Owner of `crates/asc/src/cli/json.rs`:** `mcp.md` named "U21a/U73"; the file belongs to U20 (`cli/json.rs` is in U20's Owns). The hunk that prints through `asc_rpc::json` is made by M01c, as a recorded amendment to U20. `docs/research/mcp.md` is corrected.
- **`--json` shapes:** §11.2 and §20.7 disagreed (proto3 JSON versus snake_case with hex ids). The asc JSON profile (§22.4.2) settles it for every `--json` output; `asc api` keeps standard proto3 JSON (§7). A bare-array shape is wrapped in an object, so `asc op log --json` prints `{"ops": [...]}` and `asc evolog --json` prints `{"versions": [...]}` (§20.7).
- **Server `Commit` and `ChangeMeta`:** §20.4 already requires meta for every changeset the server creates, through `IngestObserver`; M18 is the unit that makes the `Commit` handler of U48a take that path (`op = New`, `preds = []`), so changes created through MCP `commit` get a change id.
- **`search`:** the tool exists once (§22.3.2, rule M-D1). M15 depends on the `CodeIntel` proto unit I12 and is wired to the service by I21 (§23.11); until then the row is absent from `tools/list` because `Version.features` lacks `search.v1`.
- **IDE plugins** register `asc mcp` and Forgejo `/api/mcp`, and never `/mcp` next to them (§22.2 placement rule), so an agent never sees a tool twice. IDE MCP clients register through the same OAuth path as other clients: spike P50 decides whether M26's `asc-mcp` public client covers them or a second public client is needed, and the `asc-ide` token (`write:repository`, §24.6) is never handed to an agent channel [audit: ide-plugins-folded].

---

## 23. Code intelligence: search, symbols and vectors, built in [audit: codeintel-folded]

Normative from v1.7 (checklist decision "agentic code intelligence built in", 2026-10-22). This section folds `docs/research/codeintel.md` (final, reviewed): its §N is §23.(N+1) here, its decisions D1–D8 are CI-D1–CI-D8 (§23.1), and its units I01–I31 are PLAN §I. The research file stays as background and review history; where it and this section differ, this section wins. Related text: §22.3.2 (the `search` tool; this design adds no tool of its own, rule M-D1), §22.4.2 (the JSON profile), §5 (derived data), §7, §8, §15, §16.

Fixed rules this section obeys:
- no whole-repo scans, and indexing cost scales with change size;
- every index is keyed by content id (`FileId`) and deduplicated across revisions;
- read access holds inside every result list, ANN results included;
- the daemon's idle RSS stays ≤ 64 MiB, so heavy indexes live on the server (or a cache-tier replica), and the daemon serves only a bounded local subset;
- local-only mode works with a degraded local index;
- the embedding model runs locally by default, so no code leaves ASC infrastructure;
- `master`, not `main`. Code intelligence adds no IDE-specific surface; plugins reach it through the §24 bridge.

### 23.1 Decisions: vector search as the third tool [audit: codeintel-folded]

Should asc have vector search for agents? **Yes, but as the third of three tools. Exact search and symbols come first.** The point is that agents can query any revision, draft or stack **without a checkout**: all three indexes are server-side derived data that read blobs from the object store.

| Evidence | Number | Consequence for asc |
|---|---|---|
| Anthropic, Agent SDK guidance (2025-09) | "start with agentic search; add semantic search only if you need faster results or more variations" | grep/glob-style tools are primary; semantic is additive |
| Cursor, semantic search A/B (2025-11) | +12.5% Q&A accuracy (6.5–23.5%); +2.6% code retention on repos with > 1,000 files; grep-only had 2.2% more dissatisfied follow-ups | ASC has 4,543 files, above the threshold where vectors pay off |
| Zilliz claude-context (vendor) | −39% tokens, −36% tool calls, equal F1 | the gain is mostly efficiency: fewer tokens and turns |
| turbopuffer talk (vendor, 50 tasks) | file precision 65% → 87%; recall best with grep alone | agents need a **policy** for which tool to call (§23.4.4 tool descriptions) |
| Sourcegraph Cody, Continue | both dropped embeddings for search plus code graph; BM25F alone gave +20% at Sourcegraph | a good lexical ranker is the baseline; vectors must never be the only path |
| Cursor index sync (2026-01) | chunk embeddings cached by content hash; time to first query 7.87 s → 525 ms median | content addressing (our `FileId`) makes reuse free |

Decisions CI-D1–CI-D8 (the research file calls them D1–D8; the prefix keeps them apart from the decisions of other sections):

| # | Decision |
|---|---|
| CI-D1 | Three indexes, all derived data keyed by `FileId`: **grams** (doc-level trigram postings), **tags** (tree-sitter definitions and references), **chunks** (AST chunks plus embeddings). A revision is a set of `FileId`s, composed through `TreeId`-keyed doc sets (§23.2.3). |
| CI-D2 | One engine for postings and BM25: **tantivy 0.27.0**. Segments, merges, FST term dictionary, bitpacked postings and mmap come from tantivy. asc writes only the trigram tokenizer, the regex-to-trigram planner and the scope filter. No custom posting store. |
| CI-D3 | Cross-file navigation is **search-based**: reference names are joined to definition names, then ranked with import, project-graph and path signals. Stack graphs are archived and are not used. Compiler-grade SCIP is a v1.x upload path (I31). |
| CI-D4 | The default embedding model is **jina-embeddings-v2-base-code** (Apache-2.0, 161M parameters, 768 dims, 8,192-token context). It runs on the CPU through **fastembed 7.2.0**. CodeRankEmbed (MIT, 137M) and Qwen3-Embedding-0.6B (Apache-2.0) are the bake-off candidates (I28). Changing the model is a config change plus a background re-embed. |
| CI-D5 | ANN uses **usearch 2.26.5** HNSW over **i8** vectors on the server (4x smaller than f32, about 99% of quality with rescoring). Small scopes use exact brute force. The daemon's local index stores **b1** codes with i8 rescoring. |
| CI-D6 | The ACL unit is the repository namespace (SPEC §15.1, §15.5). Each namespace has its own indexes, so no ANN graph ever contains another namespace's vectors. Within a namespace, every query is filtered by a scope bitmap **inside** the traversal. A path-level deny rule, if one is ever added, subtracts a subtree bitmap in the same predicate. |
| CI-D7 | Indexing runs off the write path: an outbox effect enqueues a job, and workers drain the jobs. Readers report `coverage`. Exact search stays complete when the index lags, because the unindexed part of the scope is verified by streaming, within a budget. |
| CI-D8 | Files that look like secrets are never embedded, never sent to an external provider, and are redacted in agent-facing snippets. Exact search still finds them, as `git grep` would for anyone with read access. |


### 23.2 Architecture

#### 23.2.1 Per-content units (computed once per `FileId`, never again)

| Unit | Key | Content | Pure function of | Stored at |
|---|---|---|---|---|
| Doc row | `(ns, FileId)` → `ord: u32` | dense doc ordinal, size, flags that the bytes alone decide (`binary`, `invalid_utf8`, `too_large`, `long_line`, `too_many_grams`, `minified`, `secret`), per-index versions. No language, no attribute or config flag | file bytes only | SQL `ci_docs` |
| Doc languages | `(ns, ord, lang)` | one row per distinct language that the extension or shebang of any path holding the `FileId` gives (usually one; ≤ 4) | bytes plus path | SQL `ci_doc_langs` |
| Grams | doc `ord` in the `files` tantivy index | distinct ASCII-case-folded byte trigrams, as a multi-valued indexed `u64` field (24-bit value, doc-level, no positions) | bytes, `grams_ver` | tantivy segments (§23.2.4) |
| Tags | `d/tags.v1/<blake3(FileId ‖ lang ‖ tags_ver)>`, one object per `(FileId, lang)` | postcard `Vec<Tag { name, kind, role: Def\|Ref, range, name_range, container: u32, sig: Vec<u8>, doc: Vec<u8> }>` | bytes, grammar version, query version | object store (`d/`, evictable, recomputable) |
| Symbol terms | doc `ord` in `files` | `sym_def` and `sym_ref` raw terms; `ident` (camel/snake-split, lowercased, BM25 with frequencies); `doc` (comments and docstrings, BM25); `path_tok` (≤ 4 paths seen, ranking only); `lang` (multi-valued, from `ci_doc_langs`) | tags | tantivy segments |
| Chunk map | `d/chunks.v1/<blake3(FileId ‖ lang ‖ chunker_ver)>` | `Vec<(start_line, end_line, symbol, ChunkTextId)>` | tags plus bytes | object store |
| Chunk rows | `(ns, chunk_ord)` | `doc_ord`, `lang`, lines, `ChunkTextId = blake3::derive_key("asc v1 ci chunk", embed_text)` | chunk map | SQL `ci_chunks`; the `chunks` tantivy index (BM25 over `ident`, `doc`, `symbol`) |
| Vectors | `(ns, model_gen, ChunkTextId)` | i8 × dims | chunk text, model | vector segments (§23.2.4) |

- **Dedup:** a `FileId` gets one `ord` per namespace, however many paths and revisions hold it. An identical chunk text is embedded once per namespace and model generation (`ChunkTextId`). Embeddings are never shared across namespaces, so the rule of SPEC §3.3 against confirmation attacks also holds for indexes.
- **Byte skip rules** (doc flags; the doc gets no grams, tags or chunks): a NUL byte; invalid UTF-8; size > 1 MiB; any line > 2,000 bytes; > 20,000 distinct trigrams; the secret scan (§23.7). Only these are stored per `FileId`, because only the bytes decide them. These are the codesearch and Zoekt constants.
- **Path-dependent excludes are query-time, not stored per file:** `linguist-generated`, `linguist-vendored`, `-text`/`binary` attributes and the excludes of `.asc/codeintel.toml` depend on the path and the revision. The attribute files are read per directory with `.ascattributes` first and `.gitattributes` as the fallback (SPEC §2.4.1, checklist), never merged. The scope derivation (§23.2.3) computes the excluded bitmap `xdocs` per `(TreeId, rules digest)`, and every query uses `scope ∖ xdocs`. A file excluded at every path it has is indexed at backfill priority only; a query that sets `include_excluded` indexes it on demand. Indexing therefore never depends on ingest order, and the same `FileId` can be excluded at one path and included at another. On ASC they skip the 12 files holding 59% of the bytes (ONNX, WASM, jsonl corpora, `semantic-index.json`). Skipped and excluded files can still be found by path, and exact search verifies them by streaming when they are in scope and ≤ `core.bigFileThreshold`.
- **Embedding text** = language + enclosing symbol signature + the chunk body. The path is left out, so renames and copies keep their vectors. The path is a BM25 field and a ranking feature instead.

#### 23.2.2 Chunking (for embeddings and chunk BM25)

The chunker splits the AST, then merges (cAST: +4.3 Recall@5 on RepoEval and +2.67 Pass@1 on SWE-bench against line chunking):
1. Walk the tree-sitter tree. A node of ≤ 1,536 non-whitespace characters (about 512 tokens) is a chunk candidate.
2. A larger class, impl or function node gets a **collapsed** chunk: its signature and member signatures with bodies replaced by `{ … }`, as Continue does. The walk then recurses into its children.
3. Adjacent small siblings are merged greedily up to the budget.
4. Files with no grammar get line windows of 60 lines with 10 lines of overlap.

`text-splitter` 0.33.0 (`code` feature) was considered and rejected: it does not do the collapsed-signature step. The chunker is about 300 lines over tags plus the tree.

#### 23.2.3 Revision → set of `FileId`s (the scope), without walking the repo

| Piece | Definition | Cost |
|---|---|---|
| `tdocs.v1` (content-keyed derived value, like `mset.v1`) | key `TreeId`; value = roaring bitmap of the `ord`s of every File/Exec entry in the subtree, plus `dups`: a sorted list of `(ord, count)` for each ord that appears at ≥ 2 paths in the subtree (an ord that is not listed has count 1). Stored only for subtrees with ≥ 256 files. Smaller subtrees are expanded from their (cached) tree objects. | Derived from the children's `tdocs` plus the tree's own files. Unchanged subtrees are memo hits, so a new revision costs O(changed paths × depth) tree reads and bitmap unions. |
| `scope(rev)` | `tdocs(rev.root)` | one memo read once derived |
| Path filter | literal prefix `p` → `tdocs(lookup(rev.root, p))`; excludes subtract; globs are checked at locate time | O(depth) tree reads |
| Historical revision with no derived root | Probe the ancestors at the `skip.v1` distances (4, 16, 64) with one `has_many` for a derived `tdocs`. If one is found within 64 first-parent steps, `scope = tdocs(A.root) ⊕ Δ`, where Δ is built from the `changes` lists between them (metadata only, no new objects). A roaring set has no counts, so Δ is applied with counts: start from the `dups` of `tdocs(A.root)`, add 1 for each added path and subtract 1 for each removed path, and drop an ord from the bitmap only when its count reaches 0. Deleting one of two paths that hold the same `FileId` keeps the file. Otherwise derive `tdocs` from the tree, reusing every shared subtree. | O(log n) probes plus O(Δ), or O(changed spine) |
| `--stack` | scope = the top change's head (resolved through `ResolveChanges`) | as rev |
| `--stack-diff` | scope ∧ the ords of `paths(base..top)` (SPEC §4.5), with base = `merge_base(top, trunk)` via skip lists (SPEC §4.3) | O(log n + stack paths) |
| Workspace (daemon only) | server scope at `HEAD` minus dirty paths, plus the local overlay (§23.2.5) | O(dirty files) |
| Locate (ord → paths) | descend from the root only into subtrees whose `tdocs` contains the ord | O(depth) bitmap probes per hit |

- New `FileId`s met while deriving `tdocs` get an `ord` at once (one batched `INSERT … ON CONFLICT DO NOTHING`), and a file job is enqueued. An ord can exist before its content is indexed, so `coverage(scope, index) = |scope ∧ indexed[index]| / |scope|`.
- **Where `indexed[index]` lives:** a roaring bitmap per index, stored as an immutable bucket object `d/ciidx.v1/<blake3>` (≈ 50 KiB at 400k docs). `ci_manifest` names it (`indexed_ref`), and every manifest generation writes a new one in the same CAS. A query reads the bitmap of the generation it pinned, so coverage never overstates what that generation holds.
- The first `tdocs` of an imported head walks that head's tree once: O(files at the head), once per head ever, never per query. ASC: 4,543 files, 994 directories.
- `xdocs` is derived from the same walk: a directory whose rule digest is unchanged reuses its memo, so the cost stays O(changed spine).
- Sizes: ASC root bitmap ≈ 9 KiB. A HUGE root (400k files) ≈ 60 KiB, and a full spine ≈ 100 KiB per derived revision. Derivation is lazy (only revisions that are queried or warmed), and the values live in `d/` (evictable).

#### 23.2.4 Where each piece lives, and budgets

| Piece | Authoritative copy | Hot copy | Size (estimate; I27 measures) |
|---|---|---|---|
| `ci_docs`, `ci_chunks`, `ci_jobs`, `ci_manifest` | server SQL | – | about 60 B per doc row and 40 B per chunk row; ASC ≈ 4 MiB |
| `files` and `chunks` tantivy indexes | immutable segment files in the bucket: `d/ciseg.v1/<blake3>`. `ci_manifest(ns, index, generation, segments, model_gen)` names the live set; a CAS on `generation` publishes a commit. | each replica's `disk_dir/ci/<ns>/`, mmapped; missing segments are downloaded on manifest change | grams ≈ 0.2x of indexed text; symbols and BM25 ≈ 0.1x. ASC HEAD text ≈ 45 MiB → ≈ 15 MiB. HUGE 25 GB → ≈ 7.5 GB |
| Tags, chunk maps, `tdocs` | `d/…` objects | server cache | tags ≈ 5–10% of source |
| Vector segments | bucket `d/civec.v1/<blake3>` (usearch index plus an ord map), named by `ci_manifest` | replica disk, opened with usearch `view` (mmap) | ≈ 0.9 KiB per chunk (768 B i8 + HNSW links, M = 16). ASC ≈ 30k chunks → ≈ 27 MiB. HUGE ≈ 3M chunks → ≈ 2.7 GB |
| Server memory | `[codeintel] memory = "2GiB"` is one budget with fixed shares: tantivy writer heaps (`workers × 32 MiB` for `files` plus one 32 MiB writer for `chunks`; about 288 MiB at 8 workers), the vector-merge builder (≤ 512 MiB, §23.5), doc-store caches (256 MiB), the query-embedding LRU (64 MiB), verify buffers (`limits.text_memory`, SPEC §16.4), and the rest as headroom. Startup fails when the shares exceed `memory`. | page cache does the rest | – |
| Daemon overlay | in memory, built on first query, dropped after 5 min idle | – | ≤ 16 MiB heap; ≤ 4,096 dirty files or ≤ 32 MiB text, otherwise streaming verify |
| Local index (local-only or offline) | `$ASC_HOME/ci/<repo-id>/` (tantivy plus b1/i8 vector files under `vec/<model_gen>/`, each with a header holding the model id and calibration digest) | the daemon mmaps it on query and closes the reader after 5 min idle | capped by `codeintel.local.maxSize` (default 512 MiB); LRU by revision |

- **Consistency:** a segment commit contains whole file jobs, and `indexed[index]` is read at the manifest generation a query pins. No query sees a half-indexed file, which gives Blackbird's commit-level consistency.
- **Compaction:** tantivy's log merge policy, run by the indexing leader. Superseded segments are deleted from the bucket 10 minutes after the generation that dropped them, so pinned page tokens stay valid.
- **One writer per namespace:** a SQL lease (`ci_manifest.leader`, `lease_until`). Any replica can index file jobs into new segments, and only the leader commits manifest generations and merges.
- **Forks:** `tdocs.v1`, `tags.v1`, `chunks.v1`, `ciseg.v1`, `civec.v1`, `ciidx.v1` and all `ci_*` rows are per-namespace derived data. `Fork` and `CopyClosure` (SPEC §3.3, §7) copy the reachable content closure only and never these objects, because ords differ per namespace and a copy would carry an index into a namespace whose readers may differ. A fork starts un-indexed and backfills its own heads (I15).
- **Cache-tier role:** `asc serve --role cache` serves no CodeIntel RPCs (SPEC §7: the tier RPCs only). A region that needs local query latency runs a read replica of `asc serve`, which syncs segments from the bucket.

#### 23.2.5 Daemon and local-only

| Mode | What answers | Coverage |
|---|---|---|
| Online, workspace scope | The server answers at `HEAD`'s server revision, with `exclude_paths` = dirty paths. The daemon answers the same query over the overlay (dirty and staged files from the journal) and merges the two. | full |
| Online, any other scope | The server; the daemon only forwards | full |
| Offline, or local-only repo | The local index: grams, tags and BM25 for every `FileId` of the active workspaces' `HEAD` whose content is in `store/` or `cache/`. Built by the helper `asc codeintel worker` (a separate process, never the daemon), incrementally per local commit with the same `asc-codeintel` code. The helper is limited to: RSS ≤ 256 MiB (tantivy writer heap 64 MiB), 2 threads at low priority, disk only inside `codeintel.local.maxSize`, no network, no model. It is started only when the workspace is idle and exits after 60 s with no job. | degraded: `coverage` says how much; vectors only when `codeintel.local.embed = true` |
| Local semantic (opt-in) | The `asc-embed` helper binary, downloaded and minisign-verified by `asc codeintel enable-embed`, spawned on demand and exiting after 60 s idle. The daemon never loads a model, since a 161M-parameter model needs about 300–600 MiB. | as above |


### 23.3 Query semantics

| Query | Inputs (besides scope) | Plan | Output order |
|---|---|---|---|
| **search** literal / regex / word | `pattern`, `mode`, `case_sensitive`, `paths`, `exclude_paths`, `langs` | Port codesearch's `RegexpQuery` (exact/prefix/suffix/match sets, maxExact 7, maxSet 20, char class > 100 = any) to `regex-syntax` 0.8.11 HIR, using folded trigrams → tantivy `BooleanQuery` of `TermQuery` on `tri` → collector that keeps docs whose `ord` ∈ scope → verify each candidate with `regex` 1.13.1 over the bytes from `Objects::get` → locate. A query with no trigram (for example `.*`, or under 3 chars) is a streaming scan **of the scope**, allowed only with a path filter or a scope ≤ 2,000 files; otherwise `INVALID_ARGUMENT: pattern too broad`. | Ranked within a verified window of 2,000 files (features: symbol-definition hit, basename match, path depth, test or generated penalty), then by `ord`; cursor = last `(window, ord)` |
| **symbols def** | `name` (exact, or `prefix*`, or fuzzy with distance ≤ 1 via the term FST), `kinds` | `sym_def` term ∩ scope → load tags → filter | exact > same project (G07 project graph) > importer-near > path distance |
| **symbols refs** | `name`, optional `def` (path and line) | `sym_ref` term ∩ scope. Languages whose tags give no refs fall back to a word-boundary trigram search. When `def` is given, rank by import evidence (the ref file imports the def file's module or path) and project-graph reachability. | `confidence`: `import_match` > `name_match` |
| **symbols callers** | as refs | refs with role `Ref/call`, grouped by `container` (the enclosing definition) | by caller |
| **symbols outline** | `path` | tags of that one `FileId`; no index | source order |
| **semantic** | `query`, `mode = hybrid\|vector`, `k` | Embed the query (server, LRU keyed by `(model_gen, calibration digest, blake3(text))`, ≈ 20 ms on CPU). Choose the path by **selectivity** `s = kept chunks / namespace chunks` (kept = Σ chunk counts of the docs in `scope ∖ xdocs ∖ secret`, from `ci_docs`). If `s < 3%` or kept ≤ 20,000, run exact brute force over the kept vectors. Otherwise run usearch `filtered_search` with predicate `doc_ord(chunk) ∈ kept`, with `ef_search = min(4096, max(64, ceil(4·k / s)))` and a visit bound of `16 × ef_search` nodes. If the bound is hit with fewer than `k` results, fall back to brute force when kept ≤ 200,000 chunks, otherwise return the partial list with `ann_exhausted = true`. Each vector segment (§23.5) is searched on its own and the lists are merged. In `hybrid`, also run tantivy BM25 over the `chunks` index (`symbol` ×3, `ident` ×1, `doc` ×1, `path_tok` ×2) in the same scope and fuse with **RRF, k = 60**. Exact `sym_def` hits on query identifiers are a third list. Optional cross-encoder rerank (`bge-reranker-base` via fastembed), off by default. | fused score |

- **Every query takes a scope:** `rev` (any revspec, SPEC §4.2), `change` (a change id, resolved to its current head), `stack` (top change, with `diff_only`), or `workspace` (IPC only). The response echoes the resolved changeset and root `TreeId`, so agents can cite an immutable scope.
- **Pagination:** opaque `page_token` = postcard of `(manifest generation, scope digest, stage, last ord)`, MACed with the server key. An expired generation returns `FAILED_PRECONDITION: stale page token`.
- **Token-aware output:** the request carries `max_results`, `max_bytes` and `max_tokens`. Tokens are estimated as `ceil(bytes / 3)`, a conservative figure for code, with no tokenizer dependency. Results are grouped by file. A `FileId` at several paths is printed once with `also_at` (up to 5 paths). Long lines are cut at 300 bytes around the match. The response reports `est_tokens`, `truncated` and `next_page_token`.
- **Completeness under lag (CI-D7):** exact search verifies the unindexed part of the scope by streaming, up to 2,000 files or 64 MiB, and sets `coverage = 1`. Above that it returns partial results with `coverage < 1` and enqueues the missing files at interactive priority. Symbols and semantic return `coverage` as it is.


### 23.4 Interfaces

#### 23.4.1 RPC: `proto/asc/v1/codeintel.proto` (same port, `service CodeIntel`)

```proto
message Scope { RepoRef repo = 1; oneof at { string rev = 2; bytes change = 3; bytes stack_top = 4; bool workspace = 5; }
  bool stack_diff_only = 6; repeated string paths = 7; repeated string exclude_paths = 8; repeated string langs = 9; bool include_excluded = 10; }
message ResolvedScope { bytes changeset = 1; bytes root = 2; uint64 generation = 3; float coverage = 4; }
message Budget { uint32 max_results = 1; uint32 max_bytes = 2; uint32 max_tokens = 3; }
enum SearchMode { SEARCH_MODE_LITERAL = 0; SEARCH_MODE_REGEX = 1; SEARCH_MODE_WORD = 2; }
message SearchRequest { Scope scope = 1; string pattern = 2; SearchMode mode = 3; bool case_sensitive = 4;
  uint32 context_lines = 5; Budget budget = 6; string page_token = 7; }
message LineMatch { uint32 line = 1; repeated uint32 cols = 2; bytes text = 3; repeated bytes before = 4; repeated bytes after = 5; }
message FileHit { bytes path = 1; bytes file_id = 2; string lang = 3; repeated bytes also_at = 4; repeated LineMatch matches = 5; bool redacted = 6; }
message SearchResponse { ResolvedScope scope = 1; repeated FileHit files = 2; bool truncated = 3; string next_page_token = 4; uint32 est_tokens = 5; }
enum SymbolOp { SYMBOL_OP_DEF = 0; SYMBOL_OP_REFS = 1; SYMBOL_OP_CALLERS = 2; SYMBOL_OP_OUTLINE = 3; SYMBOL_OP_FIND = 4; }
message SymbolsRequest { Scope scope = 1; SymbolOp op = 2; string name = 3; bytes path = 4; repeated string kinds = 5;
  bytes def_path = 6; uint32 def_line = 7; Budget budget = 8; string page_token = 9; }
message Symbol { string name = 1; string kind = 2; bool is_def = 3; bytes path = 4; uint32 line = 5; uint32 end_line = 6;
  bytes signature = 7; string container = 8; bytes doc = 9; string confidence = 10; }
message SymbolsResponse { ResolvedScope scope = 1; repeated Symbol symbols = 2; bool truncated = 3; string next_page_token = 4; uint32 est_tokens = 5; }
enum SemanticMode { SEMANTIC_MODE_HYBRID = 0; SEMANTIC_MODE_VECTOR = 1; }
message SemanticRequest { Scope scope = 1; string query = 2; SemanticMode mode = 3; uint32 k = 4; Budget budget = 5; bool rerank = 6; }
message Chunk { bytes path = 1; bytes file_id = 2; uint32 start_line = 3; uint32 end_line = 4; string symbol = 5;
  double score = 6; repeated string sources = 7; bytes text = 8; bool redacted = 9; }
message SemanticResponse { ResolvedScope scope = 1; repeated Chunk chunks = 2; string model = 3; uint32 est_tokens = 4; bool ann_exhausted = 5; }
message IndexStatusRequest { Scope scope = 1; }
message IndexStatus { string index = 1; float coverage = 2; int64 lag_seconds = 3; uint64 generation = 4; string version = 5; }
message IndexStatusResponse { ResolvedScope scope = 1; repeated IndexStatus indexes = 2; }
service CodeIntel {
  rpc Search(SearchRequest) returns (stream SearchResponse);
  rpc Symbols(SymbolsRequest) returns (SymbolsResponse);
  rpc Semantic(SemanticRequest) returns (SemanticResponse);
  rpc IndexStatus(IndexStatusRequest) returns (IndexStatusResponse);
}
```

- **Rate class `codeintel`** (new, SPEC §16.3): `Search` and `Symbols` 50 req/s per principal (burst 100); `Semantic` 10 req/s per principal and 32 concurrent per replica (query embedding uses the CPU); `IndexStatus` is `cheap`. Search scans without trigrams count as `expensive`.
- **Permission:** read on every namespace searched. `Repo.Grep` (U47) stays as it is and is the streaming fallback that CI-D7 reuses.
- **Workspace IPC** (`workspace.proto`, U73 amendment): the same four RPCs; `scope.workspace = true` is accepted only over IPC.

#### 23.4.2 CLI (asc-only verbs; no git command changes behaviour)

| Command | Maps to |
|---|---|
| `asc search <pattern> [-F\|-E] [-i] [-w] [-C n] [--rev R\|--change C\|--stack [--diff-only]] [--lang L] [--json] [-- pathspec]` | `Search` (workspace scope by default inside a workspace, otherwise `master`) |
| `asc sym def\|refs\|callers <name> [--kind k] [--at path:line]`, `asc sym outline <path>`, `asc sym find <prefix>` | `Symbols` |
| `asc semantic "<query>" [--vector] [-k n] [--rerank]` | `Semantic` |
| `asc codeintel status [--rev R]`, `asc codeintel worker` (internal), `asc codeintel enable-embed` | `IndexStatus`, the local helper, the embed opt-in |

`--json` prints the response messages in the asc JSON profile `json.profile.v1` (§22.4.2: snake_case, lowercase hex ids, one JSON object per streamed message), the same shapes MCP returns; `asc api` keeps standard proto3 JSON (§7). `codeintel.proto` carries the `(asc.v1.json)` options (I12 depends on M01a). Human output follows `git grep -n` layout (`path:line:text`) with the scope header on stderr. `git grep` (U24b) is not changed. Accelerating `git grep <rev>` with the index, with byte-identical output, is the optional unit I30.

#### 23.4.3 ASC IDE plugins

The plugins call the same RPC/IPC through the generated TypeScript client (checklist, "IDE plugins"), over the daemon IPC (`Workspace`) and the server (`CodeIntel`). This design adds no separate surface.

#### 23.4.4 MCP: the `search` tool of §22.3.2 (no tool of its own)

§22 owns the MCP surface (rule M-D1: no tool exists twice). This design adds no tool name. The tools are `search` and `read_file`; the mapping onto §22.3:

| §22.3 tool | Backed by | What this design adds |
|---|---|---|
| `search` (toolset `search`, unit M15, listed only when `Version.features` has `search.v1`) | `mode: text\|regex` → `Search`; `mode: symbol` → `Symbols`; `mode: semantic` → `Semantic` | Optional fields on the M15 row, additive: `change`, `stack`, `diff_only`, `op` (`def\|refs\|callers\|outline\|find`, for `symbol`), `k`, `context_lines`, `max_tokens`, `rerank`, `include_excluded`. The response is `SearchResponse`, `SymbolsResponse` or `SemanticResponse`, with `scope`, `coverage` and `est_tokens`. The §22.3.2 `search` row carries them. |
| `read_file` (`core`) | `Repo.ReadFile` | Nothing. It expands a hit at the same revision. There is no `code_read`. |
| `grep` (`core`) | `Repo.Grep` | Nothing. It stays the streaming fallback (CI-D7). |
| index status | `ResolvedScope.coverage` in every response, and `asc codeintel status` | No `code_index_status` tool. |

The server adds `search.v1` to `Version.features` when the `CodeIntel` service is enabled. The use policy goes into the `search` tool's description: text and regex first for identifiers, strings and error messages (results are verified matches); `symbol` when a name is known (`refs` is name-based, so read `confidence`); `semantic` when the names are unknown, or after text search finds nothing (ranked chunks, not every match). Result text is untrusted file content. Defaults: `max_tokens = 4000`, `context_lines = 2`, `k = 10`. Every result carries `scope.changeset`, so a follow-up call can pin the same revision.


### 23.5 Indexing pipeline

| Step | Trigger | Work | Idempotency |
|---|---|---|---|
| Enqueue | outbox effect `codeintel.enqueue(ns, changesets)` after `UpdateRefs`, `Ingest`, `Commit` and `RestackChanges` commit (U43c amendment). It is never on the write path. | For each new changeset: the `Write` `FileId`s of its `changes` → `has` check against `ci_docs` → `INSERT ci_jobs(kind='file')` for the unknown ones only. Cost is O(changed paths). | `ci_jobs` PK `(ns, kind, key, version)`, `ON CONFLICT DO NOTHING` |
| File job | worker lease (`FOR UPDATE SKIP LOCKED` on Postgres; one in-process queue on SQLite) | read bytes once (`Objects::get`) → skip rules → secret scan → trigrams → tree-sitter tags (cancellation flag, 250 ms per file) → chunk map → write `d/tags.v1`, `d/chunks.v1` → add docs to the in-memory segment writer → enqueue an `embed` job for chunk texts not yet embedded | the outputs are pure functions of `(FileId, versions)`, so concurrent workers write identical bytes |
| Segment commit | every 2 s or 5,000 docs | tantivy commit → upload the segment files → CAS `ci_manifest.generation` → set the `indexed` bits | the CAS loser re-bases its segment set onto the new generation |
| Embed job | batches of 32 chunk texts | fastembed → i8 quantisation (per-dimension min/max, calibrated once per model on 10k ASC chunks; the table is stored in the model record with its blake3 digest) → add to the vector segment writer | key `(ns, model_gen, ChunkTextId)` |
| Merge | leader, background | tantivy merge policy. Vector segments use **tiered merges**: only segments of similar size merge, 4 at a time, and no output exceeds 512 MiB (≈ 570k chunks). The merge reads its inputs through mmap and adds their vectors one by one to a new index, so peak memory is the output segment (≤ 512 MiB, the share in §23.2.4) and never the whole namespace. HUGE (≈ 2.7 GB) settles at 5–6 segments, which queries search separately and merge. One merge runs at a time per namespace. | new generation; old segments deleted 10 min later |
| Sweep | leader, weekly | Live ords = union of `tdocs` of the warmed heads (land-branch tips, open PR heads, drafts touched in the last 30 days). An ord that has been dead for 14 days is tombstoned: deleted from the `files` and `chunks` tantivy indexes by ord, dropped from vector segments at their next merge, and its `d/tags.v1` and `d/chunks.v1` objects removed. Ords are never reused. Content that becomes reachable again is re-enqueued as new work. | idempotent; a new generation |
| Scope warm | follows `ref_log`, like `projects_warm` (SPEC §12A.5) | derive `tdocs` for new land-branch tips, `refs/pull/*/head` and draft change heads | memo |

- **Priorities:** interactive (a query met unindexed docs) > land-branch tips > PR and draft heads > backfill. Worker concurrency is `[codeintel] workers` (default: cores / 2), and workers run at lower priority (`nice` 10).
- **Backfill on import (monorepo):** index only what is needed. U33/U45 import ends with `codeintel.enqueue` for the tips of `master`, the release branches and the open PR heads, not for every historical commit. ASC: about 4.5k `FileId`s at `master` and a few hundred more for PR heads, against 49,966 blobs ever. Older revisions are indexed on demand the first time someone queries them (CI-D7 keeps exact search complete while that happens). `asc serve admin codeintel backfill --refs <glob> [--since <date>]` exists for an explicit wider backfill.
- **Model changes:** the model id is `name@<sha256 of the ONNX file>/<quantisation>/calib@<calibration digest>/<chunker_ver>`. A change to any part, including a new calibration, creates `model_gen + 1`. The reranker is pinned by sha256 in the model registry and is part of the `model` string that `SemanticResponse` echoes; it holds no vectors, so changing it needs no re-embed. The query-embedding LRU key includes `model_gen` and the calibration digest. Embed jobs for the chunks reachable from warmed heads (not history) are enqueued at backfill priority. Queries keep using the old generation until the new one reaches coverage ≥ 0.99 on every land-branch tip, then `ci_manifest.model_gen` flips atomically. The old vectors are deleted after 7 days. Vectors from two generations are never compared, and the query is always embedded with the generation it searches. **Local vectors (I20)** carry the model id and calibration digest in their file header and live under `vec/<model_gen>/`. When the server's `model_gen` (online) or the pinned local model id (offline) differs, the helper drops them and rebuilds them for the active workspace `HEAD`s; they are never migrated.
- **Extractor versions** (`grams_ver`, `tags_ver`, `chunker_ver`) bump the same way: lazy re-extraction for warmed heads only.
- **CPU path (default):** int8-quantised ONNX via ort, batch 32, threads = workers. Estimate: about 40 chunks/s per core for chunks of ≤ 512 tokens, to be measured by I27. ASC HEAD (≈ 30k chunks) on 16 cores ≈ 50 s, once. A median commit (4 files, ≈ 24 chunks) takes < 1 s.
- **GPU (optional):** `[codeintel.embed] device = "cuda"` (ort CUDA EP), or run `asc serve --role indexer` on a GPU host. The indexer role runs only workers, with no RPC serving. The CPU path is always valid; the GPU only changes throughput.
- **External providers:** opt-in only in `asc-serve.toml` (`provider = "openai-compatible"`, `url`, `allow_external = true`). `.asc/codeintel.toml` in a repo can never enable one. Secret-flagged chunks are never sent.


### 23.6 Component choices

| Need | Choice (version) | Rejected | Why, with numbers |
|---|---|---|---|
| Postings, segments, BM25 | **tantivy 0.27.0** (MIT, MSRV 1.88 ≤ our 1.95): indexed multi-valued `u64` trigram field (no positions) plus text fields | custom posting store; Zoekt-style positional trigrams; livegrep suffix arrays | Doc-level grams cost ≈ 0.2x of source (codesearch: Linux 420 MB → 77 MB); positional grams cost 3.5x (Zoekt) and suffix arrays 3–5x, and suffix arrays are hard to update incrementally. tantivy gives segment/merge/delete, an FST term dictionary and Lucene-equal BM25 for free. Its `RegexQuery` matches whole terms only, so the planner is ours. |
| Regex planner | port of codesearch `index/regexp.go` (≈ 1k LoC) onto **regex-syntax 0.8.11** HIR; verification with **regex 1.13.1** (workspace) | `lera-trigram`, `ix`, `grix` (low adoption) | No Rust crate is widely used. The algorithm is small and is property-tested against brute force. |
| Gram scheme | ASCII-case-folded **trigrams** v1 | sparse grams (Cursor, GitHub, ClickHouse) | Simplest correct choice. Sparse grams cut false positives (13-char `MAX_FILE_SIZE`: 17 grams at index time, a minimal covering set at query time) but need a bigram weight table. They become `grams.v2` only if I27 shows a candidates/matches ratio > 4 on HUGE. |
| Bitmaps | `roaring` (MIT/Apache; pin the current 0.10.x at I05) | sorted `Vec<u32>` | scope ∧ postings ∧ ACL for 400k docs in microseconds; ≈ 60 KiB HUGE root |
| Parsing and tags | **tree-sitter 0.27.1** and **tree-sitter-tags 0.27.1** (MSRV 1.90; both versions exist on crates.io); grammars: tree-sitter-rust 0.24.2, tree-sitter-typescript 0.23.2 (TS and TSX), tree-sitter-python 0.25.0, tree-sitter-c-sharp 0.23.5, tree-sitter-go 0.25.0, plus tree-sitter-javascript (pin at I03). All load through `tree-sitter-language` 0.1.9, within ABI 13–15. | stack-graphs 0.14.1 (archived 2025-09, pinned to tree-sitter 0.24, 4 languages); ast-grep as a resolver (no scopes or types) | Tags are a pure function of the file bytes, which suits caching by `FileId`. Parse plus query takes milliseconds per file. **Project-owned `tags.scm`** for all six languages, because upstream is thin (C# has about 12 patterns and no fields, properties or records; TS has 7 TS-specific patterns). A reported break of typescript 0.23.2 against core ≥ 0.25 is unconfirmed, so I03 tests it first and vendors the grammar if it fails. ASC mix: ts 2,616, tsx 84, rs 188, py 152; `surql` (287) gets grams only. |
| Embedding runtime | **fastembed 7.2.0** (Apache-2.0, `default-features = false`, so no `hf-hub` download) over **ort =2.0.0-rc.15** (ONNX Runtime 1.31; pinned exactly, because it is an RC; checked on crates.io: fastembed 7.2.0 requires `=2.0.0-rc.15`). Models load through `UserDefinedEmbeddingModel` from bytes that `asc-embed` has verified against the registry sha256 and minisign (§23.7); fastembed never fetches. fastembed's own `tokenizers` dependency enables `onig` (C code). **Decision (2026-10-22): keep fastembed's `onig` tokenizer**, with an MSVC CI job and a C entry in the SPEC §15.2 allow-list. I09 bypasses it (calls `ort` and `tokenizers` with `fancy-regex` directly) only if the Windows build fails. candle-core 0.11.0 only if a candle-only model wins I28 | Python sidecar; text-embeddings-inference | in-process Rust, ships rerankers too, DirectML/CUDA EPs. The API churns between majors, so pin. |
| Default model | **jina-embeddings-v2-base-code** (Apache-2.0, 161M, 768-d, 8k ctx, built into fastembed) | jina-code-embeddings 0.5B/1.5B (**CC-BY-NC**); nomic-embed-code (7B, GPU-class; it becomes an optional GPU-tier model) | Zero-effort, licence-clean and CPU-sized. **Bake-off (I28)** against CodeRankEmbed (MIT, 137M, CoIR 60.1, CSN MRR 77.9) and Qwen3-Embedding-0.6B (Apache-2.0, MRL 32–1024 dims). A candidate wins if it beats the default by ≥ 3 NDCG@10 on the ASC eval set within 2x embed time. Model licences are checked against an allow-list (MIT, Apache-2.0) in `asc-embed`, as cargo-deny does for crates. |
| ANN | **usearch 2.26.5** (Apache-2.0; C++ FFI, so it needs the SPEC §15.2 unsafe/FFI allow-list entry for the vector module and an MSVC CI job, and sits behind the cargo feature `ann`, off in `asc.exe`): HNSW, i8 cosine, M = 16, ef_construction 128, `ef_search` by the formula of §23.3; `filtered_search` predicate; `view` mmap | hnsw_rs 0.3.5 (graph cannot be mmapped); arroy 0.9.0 (LMDB, Annoy trees); lancedb 0.40.0 (pulls in Arrow and DataFusion) | Filtering inside the traversal is how CI-D6 holds within ANN. mmap keeps RSS as page cache. Native i8/b1 support. One immutable file per segment fits the bucket-segment model. |
| Quantisation | server **i8** (4x smaller, ≈ 97% raw and ≈ 99% with rescoring); daemon **b1** (32x, ≈ 92.5% raw and ≈ 96% with i8 rescoring of the top 200) | f32 everywhere | Augment: bit vectors plus rescoring gave 8x less memory, latency from > 2 s to < 200 ms and 99.9% parity at 100M LoC. The b1 loss depends on the model (e5-base-v2 kept only 75%), so I28 measures b1 retention for the chosen model, and the daemon uses i8-only if retention is < 93%. |
| Fusion | **RRF, k = 60** (Cormack et al. 2009), about 10 lines | learned fusion; score normalisation | model-free; equal to or better than CombMNZ and Condorcet Fuse on TREC; LanceDB and Elasticsearch use it by default |

`regex` 1.13.1 and `regex-syntax` 0.8.11 match `Cargo.lock`. All crates pass `cargo deny` as configured (MIT, Apache-2.0, Unlicense and others). Zoekt and ripgrep are **bench-only tools**, not dependencies.


### 23.7 Security

| Threat | Control |
|---|---|
| Reading another repository's code through search | Indexes are per namespace (CI-D6). The server authorises read on each namespace **before** opening its indexes. Composed repositories (gitlinks, SPEC §10.6) are searched only when the caller can read the child namespace; otherwise the gitlink subtree is not in scope and its results are absent, not redacted. Results from several namespaces are merged after each namespace has been filtered on its own. |
| Leakage inside ANN results | The HNSW graph of a namespace holds only that namespace's vectors. `filtered_search` applies `scope ∧ ¬secret ∧ ¬deny` during traversal, so out-of-scope chunks never take a result slot. Brute force for small scopes uses the same predicate. Test (I16): a deny subtree never appears at any `k`, even with an exact-duplicate query. |
| Path-level read rules (if SPEC ever adds them) | They subtract the denied subtree's `tdocs` in the same predicate. Locate skips denied subtrees, so a `FileId` that is also at an allowed path is shown only at the allowed path. |
| Existence probing through timing or dedup | No cross-namespace dedup of docs, chunks or vectors. Page tokens are MACed and bound to a scope digest. |
| Secrets in embeddings | Per-file secret scan at file-job time. Path rules: `.env*`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, `id_rsa*`, `*credentials*`, `secrets.*`. Content rules: private-key headers, `AKIA[0-9A-Z]{16}`, JWT shape, GitHub/Forgejo/Slack token prefixes, and `(key\|secret\|token\|password)\s*[:=]\s*` followed by ≥ 20 chars of entropy ≥ 3.5 bits/char. A flagged **file** is never chunked or embedded. A flagged **line** is masked in chunk text before embedding and redacted (`[redacted: possible secret]`) in MCP and `--json` snippets (`redacted = true`). Exact search still matches it (git-grep parity). **One renderer (I22) runs inside the server for every surface except the local human CLI:** MCP, `--json`, the Forgejo search page (I24), the Forgejo API (I26), `/explore/code` and the symbol panels. A redacted line loses `text` and `cols`, and context lines are scanned too. The path and line number stay, because a reader of the repository can already list the tree (git-grep parity). A flagged file shows its path only, with `redacted = true`. `--no-redact` is a CLI-side re-read through `Repo.ReadFile`; the RPC has no such field. |
| Prompt injection through indexed content | Results are data. MCP responses keep code in fields (`text`, `matches[].text`), never in the tool's prose. Control characters and ANSI escapes are stripped, lines are capped at 300 bytes, and every hit carries provenance (`repo`, `changeset`, `path`, lines, `file_id`) so the agent can cite it. Tools are read-only by default (checklist "MCP built in"), so an injected instruction cannot trigger a write through these tools. The tool descriptions tell the agent that result text is untrusted file content. |
| Untrusted PR content (parser DoS) | Tree-sitter runs with a 250 ms cancellation flag and a 1 MiB file cap. Regex verification uses `regex` (linear time) with `size_limit` 10 MiB. Planner sets are capped (maxSet 20). The rate class limits abuse. |
| Model supply chain | Model files and the reranker are pinned by sha256 in the model registry, with no runtime download by fastembed or `hf-hub` (§23.6). The client helper and the model are minisign-verified (SPEC §16.2). |


### 23.8 Forgejo fork

| Surface | Change | Data |
|---|---|---|
| Code-indexer backend `asc` (`[indexer] REPO_INDEXER_TYPE = asc`) | implements Forgejo's code-indexer interface under `modules/indexer/code/asc`. `Index` and `Delete` are no-ops, because the asc server indexes on ingest. `Search` calls `CodeIntel.Search` at the default branch. This replaces the git-grep fallback, which the asc backend cannot run (K05). | `Search` |
| Repo search page `/{owner}/{repo}/search` | mode tabs (Exact, Regex, Symbols, Semantic), a branch/tag/PR/change selector (the scope), language and path filters, and a coverage note while indexing | `Search`, `Symbols`, `Semantic`, `IndexStatus` |
| File view | click an identifier → side panel with the definition and references (with confidence), and "Outline" | `Symbols` |
| PR files tab | "Find usages in the base" for changed symbols | `Symbols` with `scope.rev = base` |
| Global `/explore/code` | fan-out over the repos the user can read, merged with RRF | `Search` per namespace |
| API | `GET /api/v1/repos/{owner}/{repo}/asc/search\|symbols\|semantic` proxies with the user's token. This is the hook for the Forgejo MCP endpoint. Responses are the server-rendered (already redacted) messages as JSON; Forgejo never re-renders raw lines. | the same messages, as JSON |


### 23.9 Benchmarks (`bench/codeintel/`, joins the O05 gate)

| Scenario | asc | Compared with |
|---|---|---|
| Full index build of HEAD (cold) | `asc serve admin codeintel backfill` | `zoekt-git-index`; `git grep` needs no index (recorded as 0) |
| Incremental index for a commit | ASC's commit sizes replayed (median 4 files, p95 35) | Zoekt delta build |
| Literal rare / literal common (`for`) / regex `\bfoo\w+Bar\b` / case-insensitive / `-w` | `asc search` at `master`, no checkout | `rg` on a warm checkout; `git grep` (worktree); `git grep <rev>` (no checkout); `zoekt` |
| Same query, scope = a draft stack | `asc search --stack` | `git grep <sha>` |
| `def` / `refs` / `callers` / `outline` | `asc sym …` | `rg -w` (name match) as the baseline |
| Semantic and hybrid | `asc semantic` | BM25-only (`chunks` index) as the baseline |

**Workloads:** small (ASC), medium (linux kernel import), HUGE (`bench/gen` for grams and symbols; an optional chromium snapshot for semantic, because synthetic text is not real code). **Metrics:** index build time per GB, index bytes per GB of source, incremental latency p50/p99, query p50/p99 (server time and end-to-end), candidates/matches ratio, server RSS, daemon RSS idle and under load, tokens returned per query, and semantic NDCG@10 and Recall@10 on the eval set (I28: about 200 ASC queries labelled automatically from history, with the PR title as the query and the changed files as relevant).

| Gate | Value |
|---|---|
| Exact search, HUGE, server p50 / p99 | ≤ 50 ms / ≤ 300 ms (the Zoekt design target is < 50 ms) |
| Exact search vs `git grep <rev>` and cold `rg` | ≥ 10x faster on medium and HUGE; not slower on small |
| Incremental index of a median ASC commit | grams and tags ≤ 2 s; with embeddings on CPU ≤ 10 s |
| Index size | grams plus symbols ≤ 0.4x of indexed text |
| Daemon | idle RSS ≤ 64 MiB with the overlay and the local reader closed; ≤ 96 MiB while serving |
| Semantic | hybrid NDCG@10 ≥ BM25-only + 5 points on the ASC eval set; otherwise `search` with `mode = semantic` defaults to BM25 mode, and the result is recorded in `docs/BENCHMARKS.md` |

### 23.10 Configuration and processes

- **`asc-serve.toml`** (also shown in §8.1):

```toml
[codeintel]                 # absent or enabled = false: no CodeIntel service, and no search.v1 in Version.features
enabled = false             # true by default once I21 has landed
memory = "2GiB"             # one budget with fixed shares (§23.2.4); startup fails when the shares exceed it
workers = 4                 # default: cores / 2; workers run at nice 10
[codeintel.embed]
model = "jina-embeddings-v2-base-code"
device = "cpu"              # or "cuda" (ort CUDA EP)
provider = "local"          # "openai-compatible" also needs url and allow_external = true
```

- **`asc serve --role indexer`** runs only the workers (GPU hosts), with no RPC serving. `--role cache` serves no CodeIntel RPCs (the tier RPCs only, §7); a region that needs local query latency runs a read replica of `asc serve`.
- **Client config:** `codeintel.local.maxSize` (default 512 MiB) and `codeintel.local.embed` (default `false`). The repo file `.asc/codeintel.toml` holds path excludes and can never enable an external provider.
- **Admin:** `asc serve admin codeintel backfill --refs <glob> [--since <date>]`.
- **Helpers:** `asc codeintel worker` (the limited local indexer of §23.2.5) and `asc-embed` (the model helper, minisign-verified by `asc codeintel enable-embed`). `asc.exe` does not link ONNX Runtime.

### 23.11 Crates, units and phases

New crates: `asc-codeintel` (extraction, planner, tantivy schema, scope algebra, ranking, rendering; depends on `asc-engine`) and `asc-embed` (fastembed, the model registry and the helper binary; built only for the server and the opt-in helper). The units are I01–I31 in PLAN §I.

**Phases (§18):** 4c after U47 (I01–I17, I22, I29); 4c-MCP after M15 (I21); 5c after U53b/U73 (I18–I20); 9c on the Go track after K10 (I23–I26); O-track (I27, I28); v1.x (I30, I31).

**Recorded amendments** (a named hunk in a file another unit owns, made by the I unit): U43c outbox gains the `codeintel.enqueue` effect (I13); U73 `workspace.proto` gains the four CodeIntel RPCs (I18); U33/U45 import completion enqueues heads (I15); U69 gains the `codeintel` rate class (I16); the §22.3.2 `search` row is already extended (M15, I21 wires it).

Required tests beyond each acceptance command:
- **I02:** `/Google.*Search/` plans to the 8-trigram AND; case-folded plans; a class wider than 100 chars becomes match-all.
- **I05:** a new revision derives at most `depth × changed paths` new `tdocs` objects; for a historical revision within 64 steps, scope = checkpoint ⊕ Δ with no tree reads. `tests/scope_oracle.rs` has a duplicate-path case: two paths hold one `FileId`, one is deleted and the file stays in scope; deleting both removes it. A file excluded by attributes at one path and not at another is in scope only at the second, and `.ascattributes` wins over `.gitattributes` in the same directory.
- **I06/I10:** a merge of 20 vector segments stays within the 512 MiB merge share, and the writer heaps plus the merge fit `[codeintel] memory`.
- **I10:** a filter that keeps 1% of a 3M-chunk fixture takes the brute-force path; a 30% filter takes HNSW and never exceeds the visit bound.
- **I10/I16:** an ACL deny and a non-readable gitlink child are absent from `Semantic` at k = 1…1000; namespace A's queries never open namespace B's segments.
- **I14:** a model change (including a new calibration) keeps serving the old generation until coverage ≥ 0.99, then flips once; the query LRU never returns an embedding from another `model_gen`; a sweep removes the ords of an abandoned draft after the grace period and keeps those reachable from an open PR head.
- **I15:** `Fork` and `CopyClosure` copy no `tdocs`, `tags`, `chunks`, `ciseg`, `civec` or `ciidx` objects and no `ci_*` rows.
- **I22:** a fixture file with an AWS key is never embedded, and its line is redacted in MCP output.

### 23.12 Rulings recorded at the fold [audit: codeintel-folded]

1. **Tokenizer:** fastembed's `onig` tokenizer is kept (MSVC CI job, C entry in the §15.2 allow-list). It is bypassed only if the Windows build fails (I09). The research critique that asked for `fancy-regex` is not applied, because fastembed 7.2.0 itself enables `tokenizers/onig`.
2. **MCP tools:** the only tools are §22.3.2 `search` and §22.3's `read_file`. There is no `code_read` and no `code_index_status`.
3. **`--json` shape (contradiction with the research file, resolved):** the research file said "proto3 JSON mapping". §22.4.2 makes the asc JSON profile the one `--json` codec, so `asc search|sym|semantic|codeintel status --json` print the profile. To make that possible `codeintel.proto` (I12) carries the `(asc.v1.json)` options and depends on M01a, and the CLI unit I17 depends on M01c. `asc api` keeps standard proto3 JSON (§7).
4. **Fork copies:** §3.3 now says that `Fork` and `CopyClosure` copy the content closure only, never derived index data (test in I15).
5. **Server role:** `asc serve --role indexer` joins `primary` and `cache` (§0, §8.1). The cache role serves no CodeIntel RPCs.
6. **Redaction:** the path and line number of a redacted line stay visible, because read access to the repository already allows listing the tree (git-grep parity); `text`, `cols` and context are removed on every non-CLI surface.
7. **Rate class:** `codeintel` is new (§16.3). An MCP `search` call is charged to the `codeintel` class (the rule of §22.5: the class of the backing RPC).
8. **Superseded audit proposals:** the earlier audit text for `tri.v1`, `sym.v1`, `emb.v1`, `trisum.v1`, `symsum.v1`, a `Search` service in `search.proto`, units X01–X09 and spikes S13/S14 for this topic is replaced by this section; no such names exist. Embedding-model choice and ANN memory are measured by I27 and I28, not by spikes.
9. **Naming:** the research decisions D1–D8 are CI-D1–CI-D8 here, so they do not collide with other sections' decision numbers.
10. **Related designs:** `docs/research/mcp.md` (consistent: M15 depends on I12 and I21 wires it) and `docs/research/ide-plugins.md` (calls `CodeIntel.Search|Symbols|Semantic|IndexStatus` through the daemon IPC and the server; its `Hello.features` mirrors `Version.features`' `search.v1`). `docs/research/stacks.md` and `docs/research/branchless.md` do not touch this section: stack and draft scopes resolve through `ResolveChanges` and the cloud heads of §21 (`scope.change`, `scope.stack_top`).


---

## 24. ASC IDE plugins: VS Code and JetBrains, built by ASC [audit: ide-plugins-folded]

Normative from v1.8 (checklist decision "ASC-built IDE plugins", 2026-10-22). This section folds `docs/research/ide-plugins.md` (final, reviewed): its §N is §24.(N+1) here, its decisions ID1–ID15 are IP-D1–IP-D15 (§24.1), its units are PLAN §P (P01a–P56, plus P57 and P58 added at the fold, §24.10) and its API additions A1–A9 are §24.8.2. The research file stays as background and review history; where it and this section differ, this section wins. Related text: §7 (`bridge.proto`, `Workspace.ReadFile`), §9.1, §9.4, §9.5, §11.2, §13.3, §15, §16.4, §17, §20.6, §22.2, §23.4.3.

**What "no IDE support" means now.** Older text (the checklist's "No IDE support", user decision (3) of v1.1) means only that asc does not rely on an IDE's built-in git and has no full `.git` projection. It does not mean "no IDE integration": ASC builds its own plugins, they replace the built-in git SCM in asc workspaces, and they are specified here. The IDEs' built-in git is not supported.

Fixed rules this section obeys:
- The plugins talk only to the asc daemon IPC, the asc server RPC (`asc.v1`) and the Forgejo API. One generated TypeScript client package (from proto) serves both IDE plugins and the Forgejo fork UI.
- They never parse human CLI output. Every value they read comes from proto messages.
- They replace the built-in git SCM. There is no full `.git` projection.
- They follow the asc efficiency budgets: lazy activation, no whole-repo scans, virtualised lists, paging.
- The default branch is `master`, never `main`. Branchless is the default (§21).
- TypeScript tooling is Bun, Turborepo, and Hono where a server is needed. JetBrains uses Kotlin and Gradle.

`stacks.md` and the fixed rules of §20 said "there is no IDE extension"; the 2026-10-22 decision supersedes that line, and both texts are updated (§20, §24.10).

### 24.1 Decisions

| # | Decision | Why |
|---|---|---|
| IP-D1 | **Two layers in each IDE.** The IDE's native API is used wherever it has a primitive: SCM status, gutters, badges, blame, hover, CodeLens, diff, merge, comments, quick pick, status bar and simple trees. Everything with a graph, drag and drop, a stack, review navigation or a chart lives in **one shared React UI, `asc-ui`**. The same `asc-ui` runs in a VS Code webview, a JetBrains JCEF tool window and as islands in the Forgejo fork. | This is the Sapling ISL model: a native SCM view plus a rich smartlog webview. One implementation of the hard UI for three hosts. Native primitives are faster, use less memory and work with the keyboard. |
| IP-D2 | **One transport: the `asc api --stdio` bridge.** Each IDE host spawns one long-lived `asc api --stdio` child. It carries length-prefixed `asc.v1.BridgeFrame` protobuf frames. The bridge routes each call to the daemon IPC (`Workspace`), the asc server (`Repo`, `Changes`, `Projects`, `Cloud`, `CodeIntel`) or the in-process engine. The routing is asc's own: the same `LocalBackend` routing as `asc mcp` (M09). | Discovery (`/.well-known/asc`), credentials (`git credential fill`), tiers, `CLIENT_TOO_OLD`, no-daemon mode and Windows named pipes already exist in Rust. Each plugin reimplementing them would duplicate the hardest code twice. The bridge relays the same messages unchanged, so the rule "talk only to IPC and RPC" holds. |
| IP-D3 | **Reads are RPCs. Writes are asc verbs run through `Bridge.Run`**, which returns the typed `OpResult` of `cli.proto` (M02). The bridge validates the argv against a per-verb flag schema (§24.4.5.1), spawns `asc <verb> --json …`, and decodes the output with `asc_rpc::json`, the single codec. | The same rule as M-D7 (§22.1). Hooks, the op log, the ref chokepoint and locks are reused exactly. TypeScript and Kotlin only ever see protobuf, never JSON text and never human output. |
| IP-D4 | **Webviews hold no tokens and no process access.** `asc-ui` calls a `Platform` interface. Its RPCs go through a host relay that forwards only the entries of the checked-in `allowlist.json` (§24.4.5.1): named RPCs, named verbs with a flag schema, and (method, path template) pairs for Forgejo. Merge, review submit and other destructive writes need a native confirmation dialog that the host builds from the parsed request. The host adds the Forgejo token; the webview never sees it. | This is ISL's security model. A compromised webview cannot merge, delete, call admin endpoints, inject git options or read credentials. |
| IP-D5 | **No proposed VS Code APIs.** The commit graph is a webview, not `scmHistoryProvider`. Timeline is our own history view. The action button becomes `acceptInputCommand` plus title buttons. | Marketplace and Open VSX builds cannot enable proposals (§24.4.9). GitLens ships the same way. |
| IP-D6 | **Replace built-in git, and be inert elsewhere.** The plugins claim only asc workspaces: a root `.git` gitfile whose gitdir contains `asc-workspace` (SPEC §9.4). VS Code: the workspace setting `git.enabled: false`, offered once and checked into ASC. JetBrains: `vcs.xml` maps the roots to `asc`. A non-asc folder costs one `stat` and nothing else. | The built-in git would otherwise drive asc through the shim and duplicate every view. A plain git repo in the same window keeps working. |
| IP-D7 | **Branchless first.** The status bar shows the change id, the stack position and the cloud state, not a branch. The primary action is "Create change" (`asc create --no-branch`; the flag is passed only when `Hello` reports branchless mode, §24.10 ruling 6), not "Commit to branch". Branches appear only as labels. The trunk is `master`. | Checklist decision "branchless by default", plus BD1–BD3. |
| IP-D8 | **Never gate a local feature behind an account.** Status, commit, stacks, graph, blame, history, undo, conflicts and affected all work signed out and offline. Only Forgejo-backed views ask for sign-in. | The top complaint against GitLens 19 and GitKraken. |
| IP-D9 | **Event-driven only.** The `Watch` stream (paths, HEAD, ref, stage, op, switch, `KIND_STACK`, `KIND_CLOUD`) is the only refresh trigger. There is no FileSystemWatcher on `**` and no timer polling of asc state. The exceptions are the no-daemon fallback (§24.6) and the Forgejo views, which have no event stream and use ETag-conditioned polls with the caps of §24.4.4 (inbox, Actions runs). | The cost of a refresh is then O(changed paths), with no repo walks. |
| IP-D10 | **Optimistic operation queue.** Each `asc-ui` mutation has a pure preview applier over the `Stacks` model. The UI shows the predicted state at once, queues verbs in order and keeps the preview until `Watch` confirms. `--dry-run` predicts conflicts before a drag is dropped. | ISL's lifecycle (preview, queued, running, optimistic, done) is why it feels instant over a CLI. |
| IP-D11 | **JetBrains: native VCS providers, but no `VcsLogProvider`.** `readAllHashes` loads the whole history, which breaks the no-whole-repo-scan rule. The graph is `asc-ui` in JCEF, as jj-idea builds its own graph. | Native providers give the Commit window, gutters, Annotate, Show History, the merge tool and Rollback for free. |
| IP-D12 | **AI stays out of the plugins.** Agents get asc through MCP. The plugin registers `asc mcp` and Forgejo `/api/mcp` with the IDE and injects no token: the IDE's own MCP OAuth flow (§24.4.8) obtains one. | stacks D12 and mcp.md M-D1. No upsell surface. |
| IP-D13 | **Bun for all TypeScript** (install, build, test, scripts), Turborepo for orchestration, Hono only for the `asc-ui` dev host and the mock Forgejo. JetBrains is Gradle (Kotlin DSL), wrapped as Turbo tasks. | One toolchain. Hono is small and runs on Bun. |
| IP-D14 | **Never cause a hydration storm.** On a virtual workspace the plugins do not walk, read or index working files themselves; every content read goes through `Workspace.ReadFile` (store, no hydration). They write exclusions for generated folders and watch the per-process hydration counters (§24.4.11). | IDE indexers, search and watchers are the main source of accidental hydration (SPEC §10.2 item 5, hydration policy). |
| IP-D15 | **Every cache and queue has a cap.** Ops queue, comment threads, op log, `Run` children, avatar and content caches all have a stated limit and debounce (§24.4.12). | Long sessions must not grow the extension host or JVM. |

### 24.2 Repo layout and the bridge protocol

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
packages/asc-fixtures/      @asc/asc-fixtures: deterministic fixture repos built with the asc CLI (§24.7.2)
plugins/vscode/            asc.asc extension: src/, test/, package.json (manifest); Bun.build bundle
plugins/jetbrains/         IntelliJ Platform plugin: build.gradle.kts, settings.gradle.kts, src/main/kotlin/asc/asc/**,
                           package.json whose scripts call ./gradlew (so Turbo orders and caches it)
```

| Package | Contents | Size budget |
|---|---|---|
| `@asc/asc-client` | protobuf-es v2 messages and Connect-ES v2 service descriptors for `repo`, `workspace`, `changes`, `projects`, `cloud`, `codeintel`, `cli`, `bridge`. Transports: `bridgeNode` (spawns and frames `asc api --stdio`), `relay`/`host` (webview ↔ host, allowlisted), `grpcWeb` (browser → Forgejo proxy, P04). The Forgejo REST client (`openapi-typescript` types generated from the fork's `swagger.v1.json`, plus `openapi-fetch`) with an injectable `fetch`. Id helpers (hex, k–z change ids). | ≤ 120 KB min, tree-shaken per consumer |
| `@asc/asc-ui` | Views (§24.3), the `Platform` interface, theme tokens (`--asc-*`), state, the ops queue | initial ≤ 300 KB gzip; each lazy view ≤ 150 KB gzip |
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
  rpc StoreToken(StoreTokenRequest) returns (StoreTokenResponse);  // git credential approve; fails NO_CREDENTIAL_HELPER (§24.6)
  rpc Events(EventsRequest) returns (stream BridgeEvent);          // connectivity changes, daemon restarts
}
message RunRequest { string verb = 1; repeated string args = 2; /* flags and positionals, split by the host; no shell */
  string root = 3; /* no stdin field: the child's stdin is the null device */ }

// HelloResponse.features is the union of (a) bridge flags: "plugin.v1" (this protocol), "json-verbs.v1" (every verb of
// allowlist.json has a --json form, P45), "reorder-todo.v1" (P03), "split-hunks.v1" (P46); and (b) Repo.Version.features
// copied from the asc server when one is configured ("search.v1" is added there when CodeIntel is enabled, §23.4.1).
// HelloResponse.asc_version is for display only. Gating uses features, never version comparison (§24.4.1).
```

Frames are `u32 little-endian length ‖ BridgeFrame`, at most 16 MiB each, so streams stay chunked. Cancellation drops the server call or kills the `Run` child (M-D7). The host restarts the bridge with backoff (0.2 s, 1 s, 5 s) and resubscribes to `Watch` with its last token.

### 24.3 Feature matrix

Seen in: **GK** GitKraken Desktop, **GL** GitLens, **GT** Graphite (CLI, VS Code, web), **ISL** Sapling ISL, **VG** VS Code built-in git, **JB** JetBrains Git. "Run x" means `Bridge.Run` of the asc verb `x` (typed `OpResult`). VS Code v1 and JetBrains v1 mark what ships in the first release of each plugin. A dash means v1.x.

#### 24.3.1 Working copy and commit

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
| Tags: list, create, delete | GK GL | – (v1.x; needs `Repo.ListRefs`, §24.8.2 API additions) | – | Run `tag` | – | – |
| Fetch, pull, push (git-remote mode only; hidden in server mode, where `sync` and `submit` apply) | GK VG JB | SCM `...` menu, status-bar sync | VCS menu actions | Run `fetch`, `pull`, `push` (`--json`: P45) | Y | Y |
| Add to `.ascignore` (from an untracked or ignored file) | GK | resource menu → appends the pattern to the nearest `.ascignore` through a `WorkspaceEdit` | `IgnoredFileContentProvider` for `.ascignore` + "Add to .ascignore" action | file edit; `Status` then refreshes via `Watch` | Y | Y |
| Signing | GK | no settings UI (out of scope, §24.3.6); a "signed" badge in commit details | same | `commit.gpgSign` is honoured by `create`/`modify`; the badge is `%G?` from `Log` | – | – |

#### 24.3.2 Stacks and branchless (Graphite, ISL)

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
| Optimistic queue (IP-D10) | ISL | `asc-ui` ops | same | Run + `Watch` | Y | Y |
| Branch labels (opt-in) | GK GL | labels on graph rows; "Name this change" | same | `Stacks.branches`, Run `create <branch>` | Y | Y |
| Share a snapshot link | – | command, copies `asc:<id>` and the web link (P47a) | action (v1.x) | Run `share` (C32) | Y | – |

#### 24.3.3 History and insight (GitLens, GitKraken)

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
| Commit graph: lanes, minimap, scroll markers, column toggles, search, keyboard navigation, WIP rows per worktree | GK GL | `asc-ui` editor panel; compact graph in the sidebar view | `asc-ui` tab | `Workspace.CommitGraph` / `Repo.CommitGraph` (C29, C31); minimap data in §24.4.5 | Y | Y |
| Commit details: message, files, diff, change, PR | all | `asc-ui` details; files open in `vscode.changes` (multi-diff) | `asc-ui` details; native diff | `ReadChangeset`, `ResolveChanges`, `Diff` | Y | Y |
| Compare two revisions, changes or stacks | GK GL JB | `asc-ui` compare + `vscode.changes` | `asc-ui` + native diff | `Compare` (C30b), `Diff` | Y | Y |
| Search commits by message, author, file, code change | GK GL JB | quick pick + graph filter | `asc-ui` graph filter | `Log` `grep`/`author`/`paths`, pickaxe (C30c) | Y | Y |
| Worktrees: list, create, open, remove, dirty, ahead/behind | GK GL | tree view "Worktrees" | `asc-ui` tab | `Workspace.Worktrees`, Run `worktree add/remove` (`--json`: P45) | Y | Y |
| Cherry-pick and revert from the graph | GK GL | graph context menu (asc-ui in P17, host wiring in P47c) | same | Run `cherry-pick`/`revert` (`--json`: P45) | Y | Y |

#### 24.3.4 Conflicts

| Feature | Seen in | VS Code surface | JetBrains surface | asc API | VSC v1 | JB v1 |
|---|---|---|---|---|---|---|
| 3-way merge editor | GK JB VG | `_open.mergeEditor` with `asc:` inputs (feature-detected); fallback: the marked file plus VS Code's built-in conflict-marker CodeLens | `MergeProvider` → native merge tool (non-conflicting changes in one click) | `Workspace.Conflicts`, `ReadFile`, `Stage` | Y | Y |
| Conflict list, in-progress banner, continue and abort | GK | SCM "Conflicts" group, title buttons, status bar | Commit window | `Status.in_progress`, Run `continue`/`abort` | Y | Y |
| Predicted conflicts before a move or restack | – | `asc-ui` badges during drag | same | `--dry-run` `OpResult.conflicts`, `Changes.Mergeability` | Y | Y |

#### 24.3.5 Review, inbox, CI, projects, search, agents

| Feature | Seen in | VS Code surface | JetBrains surface | asc API | VSC v1 | JB v1 |
|---|---|---|---|---|---|---|
| Launchpad inbox: needs your review, returned, ready, blocked, waiting, drafts, queued; pin and snooze | GK GL GT | tree view "Inbox" + status-bar count | `asc-ui` tab | Forgejo `GET /api/v1/user/asc/inbox` (C41b) | Y | Y |
| Stack-aware review: navigator, per-change diff, version picker, interdiff, "since my last review", overlap and mergeability badges | GT | `asc-ui` review panel; files open in native diff editors | `asc-ui` review tab + native diff | `Changes.Stack`/`Versions`/`InterDiff`/`Overlaps`/`Mergeability`, `Diff`; Forgejo pulls | Y | Y |
| Inline review comments in diff editors | GT | Comments API (`comments.createCommentController`) on `asc:` diff URIs | comment list in `asc-ui` (inline: v1.x, §24.3.6) | Forgejo reviews and comments API | Y | – |
| Approve, request changes, merge (stack guard 409) | GT | review panel, commands | review tab | Forgejo reviews and merge (review submit and merge: modal confirmation); `Changes.RequestMerge` | Y | Y |
| Check out a PR's stack | GT GK | command from the inbox or review | action | Run `stack get` (C26) | Y | Y |
| Actions: runs and jobs per change, log tail | GT (checks) | tree view "Actions"; logs as read-only `asc-actions:` documents | `asc-ui` tab | Forgejo Actions REST (P05) | Y | Y |
| Affected projects (uncommitted, change, stack) | – | tree view "Affected" | `asc-ui` tab | `Workspace.Affected` (`uncommitted`), `Projects.Affected` | Y | Y |
| Project graph | – | `asc-ui` panel | `asc-ui` tab | `Projects.ProjectGraph` | Y | – |
| Code search: exact and regex at any revision, change or stack | – | quick pick (streamed) + `asc-ui` results panel | `asc-ui` tab | `CodeIntel.Search` (I16, I18), fallback `Repo.Grep` | Y | Y |
| Symbol and semantic search | – | `asc-ui` search modes; `Go to asc symbol` command | `asc-ui` tab | `CodeIntel.Symbols`, `CodeIntel.Semantic`, `IndexStatus` | Y (when `Hello.features` has `search.v1`) | Y (same) |
| MCP for agents | GL | `lm.registerMcpServerDefinitionProvider` (`asc mcp` stdio + Forgejo `/api/mcp`, no token injected, §24.4.8); Cursor: `vscode.cursor.mcp.registerServer` | action "Configure agents" → Run `mcp install` | M09, M20 | Y | Y |

**Out of scope:** AI commit messages and AI compose (IP-D12), cloud workspaces shared across a team (GitKraken), GitHub, GitLab and Jira integrations, LFS (No LFS decision), git-flow, submodule editing (submodules are read-only compositions, SPEC §10.6), paid tiers, a signing settings UI (signing is `commit.gpgSign`/`gpg.*` in `asc config`; the plugins only show the badge), command palette, repo tabs, profiles and work items (GitKraken desktop features, stacks.md fixed rules).

#### 24.3.6 JetBrains v1 limits and the reason for each

| Missing in JetBrains v1 | Why | Target |
|---|---|---|
| Inline review comments in diff editors | No stable public comments API for diff editors is verified for 2026.1 and 2026.2; the review tab in `asc-ui` carries threads | v1.x, after the API survey in spike P51 |
| CodeLens-style authors and recent change | The platform's code vision is internal API in `261`; Annotate and the status-bar widget cover blame | v1.x |
| Stash, tags, project graph, visual file history, share, `stack get --worktree` | Not worth a native surface yet; stash and share are Run verbs reachable from the Stack tab later | v1.x |
| Everything a `VcsLogProvider` feeds (§24.5.4 lists them) | IP-D11: `readAllHashes` loads the whole history | not planned; the `asc-ui` graph replaces it |
| Split mode and remote development | The providers need the backend; the UI needs the frontend | after v1 (§24.5) |

### 24.4 VS Code plugin

Extension id `asc.asc`, published to the VS Marketplace and Open VSX (Cursor, VSCodium). `engines.vscode: ^1.101.0`, the first release with a stable MCP server definition provider. No `enabledApiProposals`.

#### 24.4.1 Activation and lifecycle

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
| `conflicts.ts`, `review/`, `auth.ts`, `mcp.ts` | §24.4.6–§24.4.8, §24.5 |

#### 24.4.2 SCM provider, `asc:` documents and decorations

- `scm.createSourceControl("asc", "asc", root)` per asc root. Groups: `conflicts`, `staged`, `changes`, `untracked`, hidden when empty. Each `SourceControlResourceState` carries the porcelain `xy` as `contextValue` and opens `vscode.diff(asc:HEAD, file)`, or `(asc:stage, file)` for staged entries.
- Input box placeholder: "Message (Ctrl+Enter to create a change on ‹change›)". `acceptInputCommand` = `asc.create`. `scm/title`: Create, Amend, Absorb, Undo (P26a); Restack, Sync, Submit stack (P47a). `count` = the number of changed files.
- **`asc:` URIs:** `asc:/<path>?root=<ws>&rev=<sha>`, `…&stage=<0-3>` or `…&file=<FileId>`. They are served by `Workspace.ReadFile` (P02), which works offline from the local store and hydrates through the tiers. Immutable content (rev or file id) is cached in an LRU by id (32 MiB). Stage content is invalidated by `KIND_STAGE`.
- **Quick diff:** `quickDiffProvider.provideOriginalResource(uri)` returns `asc:` at `HEAD`. "Diff against stack parent" opens `vscode.diff(asc:parent, file)`, since a second quick-diff provider would be a proposed API.
- **Stage selected ranges or a hunk:** compute the hunks of `asc:HEAD` against the buffer in the host (the `diff` package, file-local), build a `git apply --cached` patch, then call `Workspace.Stage{patch}`.
- **Decorations:** one `FileDecorationProvider`. `onDidChangeFileDecorations` fires only with the URIs from the last `Status(paths)` delta. Colours use the `gitDecoration.*` theme colours so themes keep working.
- **Status bar:** `$(git-commit) kxyzq… · 2/5 · ↓3` (change, stack position, behind `master`), a cloud indicator (P47b), and "needs restack" as a warning item. Clicking it opens the smartlog. `↓3` is the number of `master` (trunk) commits that are not in the stack's base. It comes from the `Stacks` snapshot (`WsStack.behind`, API addition A1), not from `WsStatusResponse.behind`, which compares the HEAD ref with its upstream and is empty in branchless mode. It is hidden at 0, and clicking it offers "Restack onto `master`".
- **Refresh:** `KIND_PATHS`/`KIND_STAGE` → `Status(paths)` for those paths only. `KIND_HEAD`/`KIND_REF`/`KIND_SWITCH_DONE`/`KIND_RESYNC` → full `Status`. At most one `Status` call per 100 ms per root. The resource list is capped at 10,000 entries, with a "N more, open in terminal" node (the built-in git `statusLimit` default).

#### 24.4.3 Inline blame, hover and CodeLens

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

#### 24.4.4 Tree views

All trees are `TreeDataProvider`s in the `asc` activity-bar container, except File History, which sits in Explorer. They page with a "Load more" item and fetch only when visible (`onDidChangeVisibility`).

| View | Source | Refresh |
|---|---|---|
| Worktrees | `Workspace.Worktrees` | `Watch` on any root; actions: open in new window, add (`--ephemeral`), remove, lock |
| Inbox | Forgejo inbox | on focus, and every 60 s only while visible (`If-None-Match`; a 304 costs almost nothing). The status-bar count has its own poll, §24.4.7 |
| Actions | Forgejo runs for the current stack's PRs | every 5 s only while a run is in progress and the view is visible |
| Affected | `Workspace.Affected{uncommitted}` and per change | `KIND_PATHS` (debounced 1 s) |
| Op log | Run `op log -n 50` | `KIND_OP_FINISHED` with a 500 ms trailing debounce, only while the view is visible (otherwise the view is marked stale and refreshes when shown); at most one `op log` child at a time and one per 2 s; "Load more" up to 500 entries (§24.4.12) |
| File History | `Workspace.Log{paths, follow, with_stats, limit: 50}` for the active editor | active editor change, `KIND_HEAD` |

#### 24.4.5 `asc-ui` webviews: smartlog, graph, review, history

- **Hosts:** one `WebviewViewProvider` (`asc.smartlog`) in the SCM container, and `createWebviewPanel` editor tabs for the graph, review, visual history, project graph and search. Each panel restores through a `WebviewPanelSerializer` (`onWebviewPanel:*`). `retainContextWhenHidden` is false; `asc-ui` keeps its scroll and selection in `vscode.setState`.
- **Security:** the CSP is `default-src 'none'; script-src ${cspSource}; style-src ${cspSource}; font-src ${cspSource}; img-src ${cspSource} data:; connect-src 'none'`, and `localResourceRoots` = the `asc-ui` dist. `font-src` serves the bundled codicons `woff2` from the dist. The webview makes no network request. **Avatars** are not loaded from the forge or from third-party hosts (Forgejo's `avatar_url` can point at Gravatar or another external service); the webview calls `Platform.avatar(url)` and the host fetches it: https only, no cookies and no token, public addresses only (loopback and private ranges are refused, every redirect is re-checked, at most 3 redirects), content type png, jpeg, webp or gif (never SVG), at most 256 KB, 5 s timeout, LRU of 200 entries and 2 MiB, returned as a `data:` URI. JetBrains applies the same rule in Kotlin.
- **Relay:** the webview's `asc-client` uses the `host` transport (`postMessage` of `{id, method, bytes}`). The host forwards a message only when it matches an entry of `allowlist.json` (§24.4.5.1). "All read RPCs" is not a rule: every RPC, verb and Forgejo operation `asc-ui` uses is classified there, and a unit test fails when a proto RPC or a used swagger operation is unclassified. Three request kinds: (1) asc RPCs by full name; (2) `Bridge.Run` with a verb entry whose flag schema the bridge enforces (the host runs the same check so an invalid call fails early, but the bridge is the authority); (3) Forgejo REST as `{method, path, body}`, matched against (method, path template) pairs with a body schema and a repo scope. Anything else is dropped and logged. The host never shows confirmation text supplied by the webview; it composes the text from the parsed request.
- **Theme:** `asc-ui` uses its own `--asc-*` tokens. The VS Code platform maps them from `--vscode-*` variables and the `vscode-light`/`vscode-dark`/`vscode-high-contrast` body classes.
- **Graph rendering:** rows come from `CommitGraph` pages of 200, with server-computed `column` and `edges`, so the client does no layout. DOM rows are virtualised (TanStack Virtual, overscan 20). One `<canvas>` draws the lanes for the visible window. At most 5,000 rows stay in memory (a ring keyed by page token). A `Watch` event refetches only the first page and splices rows by commit id (GitLens's ledger-diff idea, without the whole ledger). The minimap uses the header of the first `CommitGraphResponse` page (the total row count and `ref_markers[]` with row indexes), not `LogResponse.count`, because the graph and the log are different streams. If the server omits them (API addition A2), the minimap shows only the loaded pages. Keyboard: arrow keys, parent and child, next fork or ref, `/` to search.
- **Smartlog interactions:** single click selects (files of the change, no checkout); double click checks out (Run `switch`); drag reparents; buttons are disabled with a tooltip that says why ("Uncommitted changes: create or stash them first", "Frozen", "Landed").

##### 24.4.5.1 The relay allowlist (`allowlist.json`) and the bridge argv rules

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
      "stack get":   { "flags": { "--worktree": "path" }, "pos": "rev" },
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

#### 24.4.6 Conflicts

1. `Status` reports conflicted paths and `in_progress`. The SCM "Conflicts" group and a status-bar banner ("Restack paused: 2 conflicts · Continue · Abort") appear.
2. Opening a conflict calls `Workspace.Conflicts` for `{base, ours, theirs, marked}`. If `commands.getCommands()` includes `_open.mergeEditor`, it is called with `asc:` inputs (`file=<id>`), as the built-in git does (inputs swapped during a restack). `_open.mergeEditor` is an internal, unversioned command, so it is used only when present.
3. Otherwise the working file already holds the diff3 markers (`marked`). VS Code's built-in `merge-conflict` extension, which does not depend on git, adds its Accept current, incoming or both CodeLens.
4. Saving a file with no markers left offers "Mark resolved" → `Workspace.Stage{add}`. "Continue" runs `asc continue`.

#### 24.4.7 Review, inbox, Actions, affected, search

- **Review:** opening a PR (from the inbox, smartlog or a URI) opens the `asc-ui` review panel: the stack navigator, change list, version picker, interdiff and approve/merge. Files open in native diff editors over `asc:` URIs, or all at once with `vscode.changes`. A `CommentController` (`asc.review`) shows Forgejo review threads on those diff editors. New comments go into a pending review submitted from the panel. Line mapping uses the PR's `merge_base` and head (stacks D8).
- **Inbox:** the tree (§24.4.4) plus a status-bar count of "needs your review". Pin and snooze go through the inbox API. The count has its own light poll, independent of the view: `GET /user/asc/inbox?section=needs_review&limit=1` with `If-None-Match`, reading `X-Total-Count` (C41b must return it, API addition A6). It runs once on window focus and after any review action, then every 5 minutes while the window is focused and the user is signed in, and stops on blur or when `asc.inbox.statusBar` is false. The item is dimmed with a tooltip "updated N min ago" when the last success is older than 15 minutes or the forge is offline.
- **Actions:** runs, jobs and steps per change (Actions context `asc_change`, C40). A log opens as a read-only `asc-actions:` document filled by range reads (tail 64 KiB, "load earlier"). "Re-run" calls the Forgejo API.
- **Affected:** the tree lists projects with their reasons. "Run target" is a `TaskProvider` of type `asc`: one `vscode.Task` per affected project and target, `new ShellExecution(cmd, args)` with the argv and cwd from the project's target definition (`AffectedResponse.projects[].targets[<t>].argv`, API addition A4), run in the terminal panel; JetBrains uses the terminal tool window. asc does not run or schedule targets, and the task detail shows the equivalent `asc affected --target <t>` command. A project without a target definition does not offer the action. No task runner is built in.
- **Search:** "asc: Search code" is a quick pick that streams `CodeIntel.Search` (workspace scope over IPC), cancels on each keystroke and stops at 200 hits. "Open in panel" shows `asc-ui` search with modes (exact, regex, symbol, semantic), a scope picker (workspace, revision, change, stack) and the index coverage from `IndexStatus`. `search.v1` is advertised by the asc server in `Repo.Version.features` when CodeIntel is enabled (§23.4.1) and reaches the plugin as `Hello.features`. When it is absent (including every local-only workspace, which has no server), exact search falls back to the local engine's `Grep` (`Repo.Grep`) and the other modes are hidden with the reason.

#### 24.4.8 MCP registration

- `contributes.mcpServerDefinitionProviders: [{ id: "asc", label: "asc" }]`. The provider returns `McpStdioServerDefinition("asc", ascPath, ["mcp"], { cwd: root })` per asc root and `McpHttpServerDefinition("forgejo", <forge>/api/mcp)`. **No `resolveMcpServerDefinition` token injection:** the `asc-ide` token carries `write:repository`, which an agent channel must not receive. The IDE's own MCP OAuth flow discovers the authorization server from the protected-resource metadata that M20 serves (`/api/mcp` answers 401 with `WWW-Authenticate: Bearer resource_metadata=…`) and gets its own, narrower token. M20 would accept any Forgejo API token (PAT or OAuth2), but the `asc-ide` token is not reused for agents, for least privilege. Open point, owned by spike P50: M26 registers only the `asc-mcp` public client, builds no dynamic client registration and leaves client-ID metadata documents to v1.x (M27), so the IDE client may have nothing to register with. Until P50 passes, the provider declares an `inputs` entry (`promptString`, `password: true`) so the user pastes a Forgejo PAT that VS Code stores securely and sends as the bearer; the extension never reads it.
- It never also registers the asc server's `/mcp` (mcp.md placement rule: no tool appears twice).
- Cursor: when `vscode.lm.registerMcpServerDefinitionProvider` is missing, it calls `vscode.cursor?.mcp?.registerServer` with the same two definitions. Otherwise the command "Configure agents" runs `asc mcp install --scope project`.

#### 24.4.9 Proposed-API gaps and the replacements

| Built-in git uses (proposal) | asc plugin instead |
|---|---|
| `scmHistoryProvider` (Source Control Graph) | `asc-ui` graph and smartlog webviews |
| `scmActionButton` | `acceptInputCommand`, `scm/title` buttons, status bar |
| `scmValidation` | validated on accept; an error notification keeps the message in the input box |
| `timeline` | File History tree and visual history panel |
| `quickDiffProvider` (secondary) | "Diff against stack parent" command |
| `scmMultiDiffEditor` | the public `vscode.changes` command |
| merge editor API | `_open.mergeEditor` when present, else conflict markers (§24.4.6) |

#### 24.4.10 Performance budgets (enforced by P44a for VS Code, P44b for JetBrains)

| Metric | Budget | Measured by |
|---|---|---|
| `activate()` in an asc workspace | ≤ 30 ms, no awaited I/O except the marker read | test-electron timing |
| Cost in a non-asc workspace | ≤ 5 ms, no bridge spawned | test-electron |
| First SCM status visible | ≤ 300 ms after activation (ASC, warm daemon) | test-electron with the fixture |
| Extension-host heap attributable to `asc.asc` at idle | ≤ 25 MiB | The extension host is shared, so per-extension heap cannot be read in a normal window. P44a starts a test-only window (`--disable-extensions --extensionDevelopmentPath=plugins/vscode`, `--js-flags=--expose-gc`) with a probe that forces GC and reads `v8.getHeapStatistics().used_heap_size`, and compares it with the same window on a non-asc folder (the extension inert). The delta is the attributable heap; median of 5 runs |
| Bridge RSS at idle | ≤ 20 MiB | process stats |
| Webviews at idle | at most one (the smartlog view, only when visible) | test |
| Graph | first 200 rows painted ≤ 150 ms after data; scrolling 100,000 rows ≥ 55 fps; ≤ 5,000 rows in memory | Playwright trace (§24.7) |
| Event handling | `Watch` events coalesced over 50 ms; ≤ 1 `Status` per 100 ms per root | unit test with a fake clock |
| Bundles | host ≤ 350 KB min; `asc-ui` initial ≤ 300 KB gzip; each lazy view ≤ 150 KB gzip | CI size check |
| Never | `*` activation, `createFileSystemWatcher("**")`, `workspace.findFiles` over the repo, unpaged history, polling without a visible view | oxlint rule + review |

#### 24.4.11 Hydration discipline (SPEC §10.2 item 5, hydration policy)

On a virtual workspace, a read of a file that is not local hydrates it. IDE search, indexing and watchers are the usual cause. Directory enumeration and `ReadDirectoryChangesW` watchers never hydrate (SPEC §10.2 item 5); reading file bodies does.

| Rule | VS Code | JetBrains |
|---|---|---|
| The plugin itself | Content comes from `Workspace.ReadFile` (store, no hydration). It reads working files only for open editors. | Same (`AscContentRevision`). No `VfsUtil` recursive refresh. |
| Exclusions, written once at workspace scope after a prompt and checked in | `files.watcherExclude`, `search.exclude` and `files.exclude` for `**/node_modules`, `**/dist`, `**/.turbo`, `**/target`, `**/.next`, `**/.asc`; `search.followSymlinks: false`. | `DirectoryIndexExcludePolicy` for the same folders; `.ascignore` through `IgnoredFileContentProvider`. Source roots stay indexed: language servers and IDE indexers are expected to hydrate sources, and `asc clone`/`worktree add` prefetch the `.asc/prefetch` globs for that (SPEC §10.2 item 5a). |
| Find in Files | VS Code's text search reads every file it visits. The default is therefore "asc: Search code" (index-backed, no hydration, §24.4.7); the plugin never rebinds Ctrl+Shift+F. | `asc` tool window search tab; the IDE's Find in Files stays available. |
| Watching the counters | The host reads `Workspace.Stats` (never relayed to a webview) once a minute while a window is focused and `asc debug hydrations --by-process` semantics apply. If one editor process (`rg`, `Code Helper`, an indexer) caused more than `asc.vfs.hydrationBudget` hydrations in a day, show one notification per 24 hours: "Search is hydrating files. Use asc: Search code, or exclude folders" with the top three folders. | Same, from Kotlin, with a link to the excludes. |
| `asc.vfs.denyHydration` | Not changed by the plugin. The notification names the process image so the user can add it. | Same. Windows Search images are already denied by default. |

P49a and P49b test this on a virtual fixture: after activation, a window reload, a search-for-text and an indexing run, `asc debug hydrations --by-process` shows 0 hydrations from the plugin and none beyond manifests from the watcher.

#### 24.4.12 Limits and debounce

| State | Limit | Behaviour at the limit |
|---|---|---|
| `asc-ui` ops queue | 50 queued ops per root; 1 running; 20 finished ops kept for the status line | New ops are refused with "Wait for the running operations"; a queued op whose preview no longer applies after a `Watch` event is cancelled and shown as such |
| `Run` children | 4 per bridge; stderr 1 MiB per child | Others wait in order; excess stderr is truncated with a marker |
| Op log | `-n 50`, "Load more" to 500; refresh debounced 500 ms, one per 2 s, visible view only | Older entries: "open `asc op log` in a terminal" |
| Review comment threads | 200 threads per PR loaded; pages of 50; comment text 64 KiB; `CommentController` threads of a closed diff editor are disposed; at most 20 PR controllers (LRU) | "N more threads, open in the review panel" |
| `asc:` content cache | 32 MiB LRU by id (immutable content) | Evict oldest |
| Blame cache | 100 entries per root | Evict oldest |
| Avatars | 200 entries and 2 MiB | Evict oldest |
| Status resources | 10,000 entries | "N more" node (§24.4.2) |
| Graph rows | 5,000 in memory (ring) | Page tokens evicted and refetched |
| Event coalescing | `Watch` 50 ms; `Status` one per 100 ms per root | – |

### 24.5 JetBrains plugin

One IntelliJ Platform plugin for IntelliJ IDEA, Rider, RustRover, PyCharm and WebStorm. Kotlin, IntelliJ Platform Gradle Plugin 2.x, `intellijIdea("2026.1")` (Java 21), `sinceBuild = 261`, no `untilBuild`. Depends on `com.intellij.modules.vcs`. `com.intellij.modules.jcef` is declared as an optional dependency (`<depends optional="true" config-file="asc-jcef.xml">`) with a runtime `JBCefApp.isSupported()` check, because the claim that the module becomes required from 2026.2 is unverified. Spike P54 settles it against 2026.1 and 2026.2 with `verifyPlugin`. Split mode and remote development are not supported in v1; the code is kept in `vcs/` (backend-side) and `ui/` (frontend-side) packages so a later split stays mechanical.

#### 24.5.1 Components

| Class | Platform API | asc call |
|---|---|---|
| `AscBridgeService` (application service) | coroutines, `Disposable` | one `asc api --stdio` per IDE process; protobuf-java messages; `Flow` for streams |
| `AscVcs` | `AbstractVcs` (name `asc`), `<vcs name="asc" vcsClass="asc.asc.vcs.AscVcs"/>` | – |
| `AscRootChecker` | `vcsRootChecker` | `.git` gitfile + `asc-workspace` marker |
| `AscMappingActivity` | `ProjectActivity` | maps asc roots to `asc` in `vcs.xml` and drops `Git` mappings for them; one notification; also suppresses Git4Idea's "unregistered VCS root" notice for the synthetic `.git` (spike P52, §24.5.4) |
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

Not registered in v1: `logProvider` (IP-D11), `pushSupport` (asc has no separate push step for drafts; `submit` is an action), `UpdateEnvironment` (sync is an action).

#### 24.5.2 JCEF host for `asc-ui`

- Check `JBCefApp.isSupported()`. Without JCEF, the tool window shows a notice and a link to the same view on the Forgejo fork (§24.5.3 islands).
- `JBCefBrowser` loads `https://asc-ui.local/index.html`. A `CefRequestHandler`/`CefResourceRequestHandler` serves the bundle from jar resources (the `JCefImageViewer` pattern), so nothing is loaded from the network.
- **Relay:** `JBCefJSQuery` carries base64 bridge frames JS → Kotlin, and `executeJavaScript("window.ascHost.deliver(...)")` carries them back. The allowlist and confirmation rules of §24.4.5 apply unchanged (same table, generated into Kotlin from `packages/asc-client-ts/src/transport/allowlist.json` by the Gradle task `generateAllowlist`, P48; the argv rules of §24.4.5.1 are enforced by the bridge).
- **Theme:** Kotlin computes the `--asc-*` tokens from `JBColor`/`UIManager` on open and on `LafManagerListener`, and pushes them as a `theme` message. `JBCefScrollbarsHelper` styles the scrollbars.
- **Lifecycle:** the browser is created on first open and disposed when the tool window has been hidden for 10 minutes. `asc-ui` restores its state through `Platform.getPersistedState` (`PropertiesComponent`).
- **Host actions** (`Platform.openFile`, `openDiff`, `confirm`) map to `FileEditorManager`, `DiffManager` with `AscContentRevision`s, and `Messages`.

#### 24.5.3 Forgejo fork reuse

`asc-ui` is built a second time as a self-contained ESM bundle and vendored into the fork at `public/assets/asc-ui/<version>/` (P07). A Go template partial mounts an island: `<div data-asc-ui="graph" data-repo="owner/repo">`. In the browser, the `Platform` uses the `grpcWeb` transport to a same-origin, read-only proxy `/-/asc/rpc/*` (P04). The proxy checks Forgejo's repo permission, then forwards with the internal token. Writes from the web go through Forgejo's own handlers (merge, restack button). Suggested adopters: the commit graph page (C42) and visual file history; the existing Go templates stay valid.

#### 24.5.4 Degraded features and the spikes that settle them

Without a `VcsLogProvider` (IP-D11), these platform features are expected to degrade or disappear for `asc` roots. Spike P51 runs each on 2026.1 and records the real behaviour in `plugins/jetbrains/spikes/RESULTS-vcslog.md`; the list below is the starting hypothesis, not a finding.

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
| P53 | Can `AscCheckinEnvironment` support `arePartialChangelistsSupported` (partial-line commit from the Commit window) with a `ContentRevision` backed by `ReadFile` and a patch for `Workspace.Stage`? | If not within the spike, v1 ships whole-file commit plus `asc-ui` line staging, and §24.3.1 row "Stage ... selected lines" becomes `–` for JetBrains in the Commit window |
| P54 | Is `com.intellij.modules.jcef` a module id in 2026.1 and required in 2026.2? Does `verifyPlugin` pass with it optional? | Use optional if the verifier passes on both; else required from `sinceBuild` that needs it |
| P55 | Does the OAuth loopback survive a moved built-in-server port, Remote Dev, and `state`/PKCE checks? Does Forgejo accept a variable loopback port for `asc-ide` (P06)? | Built-in server if stable; else an own `ServerSocket` on a random loopback port |

### 24.6 Auth, offline and local-only

| Topic | Decision |
|---|---|
| Credential store | asc's first: `ASC_TOKEN`, else `git credential fill` for the forge host (SPEC §15.1). The bridge runs with `GIT_TERMINAL_PROMPT=0`, `GCM_INTERACTIVE=never`, `GIT_ASKPASS` unset and no editor, so `fill` never opens a Git Credential Manager dialog at startup: with no stored credential `Token` returns `NOT_FOUND` and the UI shows "Sign in". `Bridge.StoreToken` first checks that a credential helper is configured (`credential.helper` is set and is not `store`); with none (typical on Linux, WSL and Remote) it fails with `NO_CREDENTIAL_HELPER` and writes nothing. It never uses the plaintext `store` helper. Where a helper exists, the CLI, `asc mcp`, VS Code and JetBrains share one sign-in. |
| Credential fallback | On `NO_CREDENTIAL_HELPER` the host keeps the token in VS Code `SecretStorage` (key `asc.asc.token.<forge host>`) or the JetBrains `PasswordSafe`, and passes it to the bridge as `ASC_TOKEN` at spawn. The bridge is respawned when the token changes, and a host-supplied value wins over an inherited `ASC_TOKEN`. In this mode the CLI and `asc mcp` do not share the sign-in; the UI says so once and suggests `git config credential.helper`. Under Remote or WSL the bridge runs on the workspace side and VS Code proxies `SecretStorage` from the local side. Files and settings never hold tokens. |
| asc server calls | Authenticated inside the bridge. The plugin never sees the asc server token path. |
| Forgejo REST calls | Bearer token from `Bridge.Token`, attached by the host (extension host or Kotlin). Never relayed to a webview. |
| VS Code sign-in | `AuthenticationProvider` `asc-forgejo`. `getSessions` → `Bridge.Token`. `createSession` → Forgejo OAuth2 authorization code with PKCE for the pre-registered public client `asc-ide` (P06). The redirect is `${env.uriScheme}://asc.asc/auth` through `env.asExternalUri` + `onUri`, so VS Code, Cursor and VSCodium all work. The token is then saved with `Bridge.StoreToken`, or in `SecretStorage` when that fails with `NO_CREDENTIAL_HELPER`. A PAT can be pasted instead. |
| JetBrains sign-in | The same OAuth flow with PKCE, a `state` value (128 random bits, single use, valid 5 minutes; a callback is ignored unless `state` and the PKCE verifier both match) and a loopback redirect. The port is not fixed: the IDE built-in server listens on `BuiltInServerManager.getInstance().port` (63342 by default, higher when taken, different under Remote Dev), so the redirect is built from the live port, and P06 must accept any port on `127.0.0.1` for `asc-ide` (RFC 8252 §7.3). If the built-in server is unavailable the plugin opens its own `ServerSocket` on `127.0.0.1`. Spike P55 verifies both. A pasted PAT is the alternative; it is stored in `PasswordSafe` (or `StoreToken`) and never in settings. |
| Scopes | `read:user`, `read:repository`, `write:repository` (submit goes through asc, but merge goes through Forgejo), `read:issue`, `write:issue` (reviews and comments). `asc-ide` is a separate client from `asc-mcp` (M26). |
| Signed out | Every local feature works (IP-D8). Forgejo views show one "Sign in to Forgejo" button. No nags and no trials. |

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

Disabled controls always say why (IP-D8). The UI never blocks on the network: cheap RPCs time out at 2 s, expensive ones show progress and can be cancelled. When the daemon is absent (`ASC_NO_DAEMON` or plain mode with no daemon), `Watch` is unavailable. VS Code then uses one `createFileSystemWatcher` per root with the `.ascignore`/`.gitignore` excludes and a 1 s debounce. This is the only file watcher in the design.

### 24.7 Testing

#### 24.7.1 Layers

| Layer | Tool | Scope | When |
|---|---|---|---|
| Bridge (Rust) | `cargo test -p asc --test bridge --test bridge_run --test bridge_auth` | framing, routing, cancel kills `Run` children, no-daemon path; argv injection corpus (`-c`, `-C`, `--exec`, `--exec-path`, aliases, `-x` after `--`, values starting with `-`, stdin attempts) is rejected, and every allowlisted verb has a `--json` form; `Token` never prompts and never leaves stdout unrequested; `StoreToken` fails with `NO_CREDENTIAL_HELPER` and writes nothing when no helper is set | PR |
| `@asc/asc-client` | `bun test` | transports against a real `asc api --stdio` on a fixture; the relay allowlist; Forgejo client against the mock | PR |
| `@asc/asc-ui` logic | `bun test` | preview appliers equal the real result on fixture DAGs (apply verb, re-read `Stacks`, compare); ops queue ordering; graph ring buffer | PR |
| `@asc/asc-ui` UI | Playwright (Chromium) against `asc-ui-devhost` | each view's flows; axe accessibility; screenshots in light, dark and high contrast; keyboard-only navigation; graph performance trace (100,000 rows) | PR |
| VS Code | `@vscode/test-cli` + `@vscode/test-electron` (Mocha in the extension host) on the bundled extension, VS Code stable and the `engines` minimum, Windows and Linux | SCM groups and counts, decorations, blame text, `asc:` content, commands produce the expected `OpResult`, conflicts flow, relay rejects non-allowlisted RPCs, verbs, flags and Forgejo paths and shows the modal for each `confirm` entry, activation budgets, inert in a non-asc folder | PR |
| VS Code packaged smoke | `bun plugins/vscode/scripts/smoke-vsix.ts --editor vscodium` (also `--editor vscode`; Cursor's engine is covered by pinning its VS Code version) | installs the built VSIX headless, opens the `dirty` fixture, asserts activation, the SCM group counts through a probe command, and inertness in a non-asc folder | PR (P35b) |
| VS Code budgets | `bun bench/plugins/collect.ts --target vscode --check` | the budgets of §24.4.10, with the heap probe in a test-only extension-host window | PR (P44a) |
| JetBrains providers | Gradle `test` with `HeavyPlatformTestCase` on the fixture | `ChangeProvider` output, annotation, history paging, merge data, checkin | PR |
| JetBrains UI | IntelliJ IDE Starter + Driver (`runIdeForUiTests`) | Commit window commit, Annotate, merge dialog, `asc` tool window loads `asc-ui` and shows the stack | nightly |
| JetBrains compatibility | `verifyPlugin` (Plugin Verifier) against IU, RD, RR, PY and WS 2026.1 and 2026.2 | API compatibility | PR |
| Forgejo islands and proxy | Go tests (P04, P07) + Playwright against the docker-compose Forgejo | proxy allowlist and permission checks, island mount | PR (Go), nightly (Playwright) |
| Mock Forgejo contract | the same requests against the mock and the real fork | the mock does not drift from the fork's swagger | nightly |

#### 24.7.2 Fixture repos (`@asc/asc-fixtures`)

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

### 24.8 Units, phases and asc API additions

#### 24.8.1 Units, phases and recorded amendments

The units are P01a–P58 in PLAN §P. Code lives in `packages/` (TypeScript: `asc-client-ts`, `asc-ui`, `asc-ui-devhost`, `asc-fixtures`), `plugins/vscode`, `plugins/jetbrains`, `proto/asc/v1/bridge.proto` and `crates/asc/src/bridge/` (§24.2). There is no new Rust crate: the bridge is a module of the `asc` crate.

| Group | Units |
|---|---|
| Rust: bridge, plugin API and CLI forms | P01a–P01c, P02, P03, P45, P46, P57 |
| Forgejo fork (Go) | P04–P07, P58 |
| TypeScript tooling and `@asc/asc-client` | P08–P12 |
| `asc-ui` (React; shared by VS Code, JetBrains and the fork) | P13–P23, P56 |
| VS Code extension | P24–P35c, P47a–P47c, P49a, P50 |
| JetBrains plugin | P36–P43b, P48, P49b, P51–P55 |
| Budget gates | P44a, P44b |

**Phases (§18):** 5p (the bridge and plugin API units, after U50, U52, M02, M09, U73 and C31), 9p (the Forgejo units, on the Go track after K06, K07, K14, M24, M26 and C41b), 10 (VS Code and the `asc-ui` views) and 10j (JetBrains).

**Recorded amendments** (a named hunk in a file another unit owns, made by the P unit; built units change only through a follow-up unit, so the hunks in U41's files are made by P57):

| Owning unit and file | Hunk | Made by |
|---|---|---|
| U50 `crates/asc/src/cli/api.rs` | the `--stdio` flag dispatching to `crate::bridge` | P01a |
| U73 `proto/asc/v1/workspace.proto`, `docs/plugin-api.md` | `rpc ReadFile(WsReadFileRequest) returns (stream ReadFileResponse)` (§7); one doc row | P02 |
| C14 `crates/asc/src/stack/cli/reorder.rs` | `--todo <file>` dispatching to `reorder_todo.rs` | P03 |
| M02 `proto/asc/v1/cli.proto` | `StackTodo { repeated TodoItem items }` (`pick`, `fold`, `drop`, `reword`, ordered bottom to top) | P03 |
| M02 `proto/asc/v1/cli.proto` | `--json` result messages (each an `OpResult` with typed fields) for `restore`, `switch`, `cherry-pick`, `revert`, `clone`, `worktree add\|remove\|list`, `stash`, `fetch`, `pull`, `push` | P45 |
| M02 `proto/asc/v1/cli.proto` | `SplitSpec { repeated SplitPiece pieces }`, `SplitPiece { string message; bytes patch; repeated bytes whole_files }` (git-apply patch per piece; the last piece takes the remainder) | P46 |
| C13 `crates/asc/src/stack/cli/split.rs` | `--hunks <spec>` dispatching to `split_hunks.rs`; the interactive `--by-hunk` is unchanged and never run by a plugin | P46 |
| U20 `crates/asc/src/dispatch.rs` | honour `ASC_NO_ALIASES=1` (A5) | P01b |
| U20 `crates/asc/src/cli/common.rs` | `--end-of-options` accepted by every verb of `allowlist.json` (A7) | P01b |
| U41 `proto/asc/v1/repo.proto` (built; with C17's block), C29 `crates/asc-serve/src/commit_graph_rpc.rs`, C31 `crates/asc/src/daemon/plugin_api_stack.rs` | `CommitGraphResponse.total` and `ref_markers[]` on the first page (A2); the meaning of `WsStack.behind` (A1) | P57 |
| U41 `proto/asc/v1/projects.proto` (built), G01 `crates/asc-graph/src/config.rs` | `AffectedProject.targets` and `[[project]] argv` (A4) | P57 |
| C41b `routers/api/v1/user/asc_inbox.go` | `section`, `limit`, `X-Total-Count`, `If-None-Match` (A6) | P58 |

**Order.** P01a, P01c, P02, P03, P45 and P46 follow their Rust dependencies; P01b needs P45. P08–P14 can start as soon as P01a exists (P09 needs P01b and P01c). The VS Code release is P24–P35c, P44a, P47a–P47c, P49a and P50 plus the `asc-ui` views they embed (P15–P23, P56); it never waits for JetBrains. JetBrains: P36 needs only P08 and P01a–P01c, so the spikes P51–P55 and the provider units P37a–P40, P42, P48 and P49b need no `asc-ui` view and can start right after it (gated by their spikes: P37a by P52, P38 by P53, P42 by P55). P41a needs P15, P17, P19, P48 and P54; P41b and P43a need P41a. The JetBrains release gate is P44b. The Forgejo units P04–P07 run on the Go track after K07, K14 and M26.

Required tests beyond each acceptance command are the layers of §24.7.1 (bridge injection corpus, relay allowlist, preview appliers, hydration, budgets).

#### 24.8.2 asc API additions the plugins need

| # | Addition | Used by | Where it lives | Made by |
|---|---|---|---|---|
| A1 | `WsStack.behind` (field 3 of §20.6's `WsStack`, already defined) means the number of trunk commits that are not in the stack base, capped at 999; 0 when the base is the trunk tip. There is no `behind_trunk` field (§24.10 ruling 3). | status bar `↓N` (§24.4.2) | §20.6 | P57 (test), C31 |
| A2 | The first `CommitGraphResponse` page carries `uint64 total = 3` (rows) and `repeated RefMarker ref_markers = 4` (`message RefMarker { string name = 1; uint64 row = 2; }`); the server may omit both when the total is not cheaply known, and later pages never carry them | minimap (§24.4.5) | §20.5, §20.6 | P57 |
| A3 | `Repo.ListRefs` with kind `tag`/`branch`, paged | tags view (v1.x) | §7 | none in v1 |
| A4 | `message TargetCmd { repeated string argv = 1; }` and `AffectedProject += map<string, TargetCmd> targets = 5`; source: `argv = { <target> = ["bun", "run", "test"] }` in a `[[project]]` of `.asc/projects.toml`, run with the project root as cwd. asc never runs it (`asc run` stays out of scope, §0) | "Run target" task (§24.4.7) | §12A.8 | P57 |
| A5 | `ASC_NO_ALIASES=1` disables `alias.*` resolution and external `git-<name>`/`asc-<name>` lookup | bridge argv rule 1 | §11.4 | P01b |
| A6 | `GET /user/asc/inbox?section=&limit=` returns `X-Total-Count` and honours `If-None-Match` | inbox count poll (§24.4.7) | §20.8, C41b | P58 |
| A7 | `--end-of-options` accepted by every verb of `allowlist.json` (git 2.55 does; checked per asc verb by P01b's tests) | bridge argv rule 3 | §11.1 | P01b |
| A8 | `--json` forms for the verbs of P45, and `split --hunks <spec>` with `SplitSpec` (P46) | `Run` | §11.2, M02 | P45, P46 |
| A9 | `HelloResponse.features`: `plugin.v1`, `json-verbs.v1`, `reorder-todo.v1`, `split-hunks.v1` | minimum-feature check (§24.4.1) | §7 `bridge.proto` | P01a |

### 24.9 Risks and spikes

| Risk | Mitigation |
|---|---|
| `Bun.build` CJS output is experimental | the test-electron suite runs on the bundle; `scripts/bundle.ts` swaps to esbuild in one file |
| `_open.mergeEditor` is internal and unversioned | feature-detected; the marker fallback is fully functional |
| Cursor and VSCodium lag the VS Code engine | spike in P35b: install the VSIX on the current Cursor and VSCodium; lower `engines` only if a stable API used here is missing |
| JCEF unavailable (some Linux setups, remote IDEs) | a notice and the Forgejo island fallback (§24.5.2) |
| Bridge process crash | restart with backoff, `Watch` resumes from its token; the UI shows "reconnecting" |
| Kotlin and TypeScript disagree on the relay allowlist | one `allowlist.json`, generated into Kotlin at build time (P48); the bridge schema is tested against it (P01b) |
| A compromised webview or a bad argv reaches git options | the bridge, not the host, owns the flag schema; `--end-of-options`; no stdin; injection corpus in `bridge_run` (P01b) |
| IDE MCP clients cannot register with Forgejo (no DCR; CIMD is v1.x) | spike P50; the PAT `inputs` fallback of §24.4.8 ships if it fails |
| JetBrains degrades without `VcsLogProvider`, shows the synthetic `.git` as an unregistered root, lacks partial-line commit support, or has a different `jcef` module rule | spikes P51–P54; features listed in §24.3.6 and §24.5.4 |
| OAuth loopback port moves or the redirect is not accepted | spike P55; own `ServerSocket` fallback |
| Search and indexers hydrate a virtual workspace | exclusions, `asc: Search code`, per-process counters (§24.4.11, P49a/b) |
| Publishing to the Marketplace and Open VSX cannot be proven by a local command | `release-check.ts` covers everything but the token step; the first tagged release is watched by hand and recorded in the CHANGELOG |

### 24.10 Rulings recorded at the fold [audit: ide-plugins-folded]

1. **Names and numbers:** the research decisions ID1–ID15 are IP-D1–IP-D15 here, so they do not collide with other sections' numbers. The unit ids are the research file's own (P01a–P56), so `docs/research/mcp.md` and `docs/research/codeintel.md` keep pointing at the right units (codeintel.md names P09 and P23).
2. **Superseded audit proposal (contradiction, resolved):** the audit entries `ide-plugins-unspecified-and-contradicted` and `checklist-ide-plugins-missing` proposed units IP01–IP08, directories `ide/vscode` and `ide/client-ts`, Node HTTP/2 and a Kotlin client straight over the daemon's named pipe or Unix socket, a `tonic-web` layer on `asc serve`, a new spike S13 for the JVM pipe, a fallback 127.0.0.1 daemon listener with a `$ASC_HOME/run/ipc-token`, and budgets of 300 ms activation and 150 MiB. This section replaces all of that: one `asc api --stdio` bridge per IDE process (§24.2), so neither Node nor the JVM touches the pipe and the daemon opens no TCP listener; the browser reaches asc through the Forgejo proxy P04 (gRPC-Web to the browser, plain gRPC to `asc serve`; no `tonic-web`); directories are `packages/` and `plugins/`; the budgets are those of §24.4.10. The audit's S13 is not created (S13 is the branchless spike, §19). The audit's rule "never dirty a tracked file" is not adopted: ASC checks `git.enabled: false` into `.vscode/settings.json` after a one-time prompt (IP-D6).
3. **A1 (contradiction with §20.6, resolved):** the research file adds `WsStacksResponse.stacks[].behind_trunk`, but `WsStack.behind` (field 3, `asc sl --json` "behind") already exists. No field is added; A1 defines `behind` as the count of trunk commits not in the stack base, capped at 999. The status bar `↓N` reads `WsStack.behind`, not `WsStatusResponse.behind` (which compares the HEAD ref with its upstream and is empty in branchless mode).
4. **`stack get --worktree` (contradiction with §20.7, resolved):** the research allowlist typed `--worktree` as a boolean, but §20.7 gives it a path argument (`--worktree <path>`). The allowlist entry is `"--worktree": "path"`, a path inside the opened root's parent after canonicalisation; the plugin passes the path it chose.
5. **A4 (gap in §12A, resolved):** `[targets.*]` in `.asc/projects.toml` define inputs only, so there was no source for a target's argv. A4 adds an optional per-project `argv` table. asc stores and serves it and never runs it, which keeps `asc run` out of scope.
6. **Create under a non-branchless repo:** IP-D7 says the primary action is `asc create --no-branch`. §20.7 makes `--no-branch` the default only in branchless mode, so the plugin passes `--no-branch` only when `Hello` reports branchless; otherwise it runs plain `asc create`, and the branch template names a branch as the user configured.
7. **Dependencies added at the fold:** P57 (A1, A2, A4) and P58 (A6) own the API hunks that the research file left to unnamed owners (§24.8.1 amendments table). P17 depends on P57 (minimap data), P32 on P57 (target argv) and P31b on P58 (inbox count). Hunks in the built U41 files are made by the follow-up unit P57, never by editing U41's brief.
8. **Alias and argument-frame owners:** the research file left "CLI alias resolution" without an owner. It is U20's `dispatch.rs`; the `--end-of-options` hunk (A7) is in U20's `cli/common.rs`; both are made by P01b.
9. **MCP (consistent, one sentence added):** the plugins register `asc mcp` and `/api/mcp` and never `/mcp` (§22.2 placement rule). IDE MCP clients register through the same OAuth path as other clients; spike P50 decides whether the `asc-mcp` public client of M26 covers them or a second public client is needed. The `asc-ide` token (`write:repository`) is never handed to an agent channel (§22.9 note added).
10. **Hydration references:** the research file cites "SPEC line 1284" and "SPEC 5a"; they are §10.2 item 5 and item 5a. Its cite of `codeintel.md:207` is §23.4.1 (`Version.features` has `search.v1`).
11. **Old wording rewritten:** every statement of "no IDE support" in SPEC, PLAN and `stacks.md` now says that the IDEs' built-in git is not supported and ASC-built plugins are; the full `.git` projection stays out of scope. The `[audit: user decision No-IDE]` tags stay, because they name the history of the decision.
12. **Related designs:** `docs/research/stacks.md` (rows that marked IDE UI as OUT are corrected), `docs/research/mcp.md` (placement rule and M26 note), `docs/research/codeintel.md` (the plugins call `CodeIntel` through the bridge; `Hello.features` mirrors `Version.features`' `search.v1`) and `docs/research/branchless.md` (BD1–BD3 are the basis of IP-D7; the cloud status item and `Watch KIND_CLOUD` are §21.3).

---

## Appendix A. Judge flaws and how they are fixed

| Flaw (judge) | Fix |
|---|---|
| Placeholder mtime = committer time; needs per-path history; can be older than build outputs (J1–J3) | Per-placeholder switch-epoch time, monotonically increasing, stored in `entry_times` and the index (§10.1) |
| `asc commit` ingests unknown staged SHA-1 from the working file (J1–J3) | `asc add` ingests content when staging; commit reads no content (§9.5, §9.7) |
| Stage 2 pre-receive ran before new commits existed in git (J1) | Export into a bridge quarantine before pre-receive; promote after the CAS (§8.3 steps 6–8) |
| No transactional outbox (J1) | Ref CAS, ref log, op result and outbox in one SQL transaction; at-least-once worker (§8.3) |
| CI host cache read-write; no verify-on-read (J1) | Host cache read-only in jobs, per-job writable cache, trusted warmer; BLAKE3 verification on every read (§12.3, §3.2) |
| Write-behind leaks secrets; GC can lose drafts (J1/perf) | Superseded by the user decision "branchless by default" (§21): upload is off on public repos and behind a pre-upload ignore and secret hold, with owner purge, cache eviction and quota (§21.3.7); `store/` keeps un-uploaded objects of visible heads (§3.5, §21.2.4) |
| Land request state not atomic with the CAS (perf) | `op_results` in the same transaction; no queue/leader (§8.5) |
| Importer renamed case-colliding paths (perf) | History imported as-is; colliding/invalid names are not projected on Windows (§2.8) |
| Client-supplied `git_oid`/size trusted (perf) | Server recomputes `file_meta` and verifies every entry and change (§8.4) |
| Too many crates and moving parts (perf/integration) | 12 crates plus the test-only `asc-sim`, one binary, no land queue, no segments, no WatchRefs (§0) |
| Two storage formats in server vs serverless (perf) | One bucket layout everywhere (§3.3) |
| LFS content vs pointer identity ambiguity (integration) | Superseded by the No-LFS user decision: pointer files are ordinary blobs (§2.5) |
| `asc adopt` in place leaves Full files (integration) | No in-place adopt; `clone --reference` into a new workspace; the user deletes the old checkout (§6.3, §14.3) |
| Submodule composition with nested writes on the critical path (integration) | Read-only composition, after the MVP; folding is a user decision (§10.6) |
| Serverless refs single-machine (J2/J3) | `BucketRefs` with ref-log CAS (create-only); probe; explicit single-writer fallback (§3.4) |
| Linux FUSE and virtual CI deferred (J2/J3) | FUSE in phase 7 with plain mode as the measured CI default; CI meets "no fetch until accessed" for history and submodules, and virtual CI is gated on measurement (§12.3) |
| Forgejo shows asc ids at Stage 2 (J2/J3) | Separate `object_format_name` switch; `id_mode=GIT` at the boundary (§13.2) |
| Soma folded by fiat (J2) | Not folded; it is a user decision (§19) |
| Root/tree format coupling unstated (J3) | Stated and accepted; the v1 tree format has no size limit (§0 decision 3) |
| `git-remote-asc` off the critical path (J3) | Superseded: the No-IDE user decision removes `git-remote-asc` (§9.6) |
| Optimistic schedule; units too coarse (J3) | 5-month plan; the CLI is split into command-family units; the server is split into core, forgejo, mirror, land, reads, mutations and turbo units (PLAN) |
| Missing: tombstones, directory renames, signatures after rebase, husky hooksPath, Windows-invalid names, PutMode fallback, bucket GC/ABA/reflog, indexer/AV storms, node_modules placement, cross-fork reads, IDE `git commit`, backup/DR, distribution and skew, watchers, daemon crash, patch-stack cost, onboarding, concurrent index writers, autocrlf, Claude Code worktrees, MAX_PATH, rolling upgrades | §10.2 items 6/8, §8.5 step 4, §9.4.1, §2.8, §3.3, §3.4/§3.6, §10.2.5, §10.5, §3.3, §9.6 (superseded by No-IDE), §15.3, §16.2, §10.2.5, §9.1, §13.3, §11.3/§18, §9.4, §2.4.1, §9.3, §10.2.10, §2.2a/§16.2 |

**Round-1 audit fixes (v1.1)**

| Gap | Fix |
|---|---|
| User decision No-IDE (checklist, after the audit) | Synthetic `.git` for turbo only, asc-owned staging, no import of `git commit`, no `git-remote-asc`, plugin API (§0, §7, §9.4–§9.7) |
| git-shim-argv0, git-shim-install-as-git | §11.4 shim and `RealGit` |
| unsupported-flags-delegate | §11.5 native table; full pretty-format and ref-filter sets (§11.1). Real-git fall-through inside asc workspaces is not possible under No-IDE, so unsupported invocations fail with exit 129 |
| compat-suite-gate | §17.1, §18 exit criteria |
| display-ids, display-ids-contradiction | §11.1 (git SHA-1 by default, `core.abbrev`), §2.1 |
| error-wording | §11.6 |
| git-state-files | §4.4 in-progress state in git's format |
| hooks-full-set | §9.4.1 |
| worktree-layout-and-git-worktree | §9.3 branch-in-use and nested paths (layout change not needed under No-IDE) |
| claude-code-worktrees | §9.3 |
| remote-url-projection | §9.4 config |
| push-land-semantics, git-push-land-tracking-refs | §8.5 (land only on request; legacy pushes fast-forward) |
| git-env-vars | §11.1 |
| includeif-identity | §9.4 |
| gitattributes, gitattributes-normalisation | §2.4.1 |
| master-default, default-branch-master | §6.5 |
| branch-scope-model, 6-ref-scoping | §6.1, §2.7 |
| diff-byte-equality | §4.3 |
| projgraph (all project-graph gaps) | §12A, §5, §7, §12.2 |
| 9-ingest-without-ref | §8.7 |
| 2/12-history-fetch | §6.3 |
| 16-stored-bytes-serving | §3.2 |
| 17-tier-contract, 15-seed-snapshots (now v1) | §7 cache role, §12.3 seeds |
| 18-overload-isolation, 24-rate-limiting | §16.3 |
| 19-hit-rate-slo | §16.4 |
| 14-node-daemon-mapping | §9.1 |
| 10-derived-trait-shape | §5 `DerivedKeyed` |
| 5-ingest-cost | §8.4 |
| traceability-21-32, scion-checklist-traceability | §1, §1.1 |
| 21-metadata-first | §18 M-meta |
| 22-ingest-invariants | §14.4 |
| 23-landing-index | §8.9 |
| 30-ancestry-path-indexes | §5 `skip.v1`, §5.1 |
| 24-fuzzing, 24-hermetic-simulation | §17.3, §17.2 |
| 25-trust-boundaries | §15.4 |
| 26-directory-permissions | §15.5 |
| 27-semantic-classification | §8.10 |
| 28-agent-conflict-api | §8.11, §8.5 step 3, §12.6 |
| 29-observability-superfans | §16.1 |
| 31-api-standardisation | §7 |
| projfs-reset-restore-dirty-set, watchers-virtual-switch, projfs-dir-tombstone-reconcile, ls-crawl-hydration, redirection-mechanics | §10.2, §10.3, §10.5 |
| land-rewrite-signature-and-raw-extra | §8.5 step 5a, §4.4 |
| gc-sweep-race-and-pitr-roots | §3.6, §15.3 |
| rolling-upgrade-encodings | §2.2a, §8.2, §9.2, §16.2 |
| agit-refs-pull-stage2 | §13.5 |
| upgrade-mechanics-windows | §16.2 |
| daemon-hang-watchdog | §9.1 |
| crate-cycle-statedb, historyapi-cycle, sqlx-two-migrators | §3.5, §4.3, §9.2 |
| bucketrefs-defer | §3.4 |
| Not applicable under No-IDE: ide-git-fetch-pull, refs-mirror-locking, git-worktree-add-in-projection, 20-projection-odb-maintenance, 32-helper-negotiation | §9.6 (no projection ODB, refs mirror or helper); item 32 in §1.1 |
| Later checklist additions: `.ascignore`, Actions fork, benchmark vs git, checkout speed tiers, No LFS | §2.4.2, §12.7, §16.4, §2.5 |

**Round-2 audit fixes (v1.2)**

| Gap | Fix |
|---|---|
| commit-precommit-staging-order | §9.7 hook order (pre-commit → re-read stage set → message hooks → build); §9.4.1 `GIT_INDEX_FILE` |
| git-index-file-stage-sets | §9.5 `GIT_INDEX_FILE` → stage set; §9.2 `stage.set_id`, `stage_sets` |
| hook-child-path-real-git, hook-child-path-shim | §9.4.1 child PATH (private shim dir `<exe dir>/shim`; Git for Windows `usr\bin\sh.exe`) |
| windows-shim-path-precedence-and-location | §11.4 dispatch (shim in `<prefix>/<ver>/shim`), machine-PATH insertion, `asc doctor` probes; §3.5 `bin/` removed |
| repo-independent-commands-routing | §11.4 target-based routing |
| compat-fixture-routing | §11.4 `asc.shim.init`; §17.1 asc-side fixtures and `ASC_SHIM_TRACE` |
| clone-history-o-history | §6.3 history window and `history_boundary`; §4.3; §6.4; §12.3 seeds; §9.2 |
| ci-shallow-semantics | §12.3 step 4 (no `shallow`); §9.4 layout |
| synthetic-index-o-files | §9.4 `asc.turboIndex`, incremental patching; §16.4 gate |
| plugin-api-undefined | §7 workspace.proto messages; §11.2 `--json`, `asc watch` |
| affected-default-base-contradiction | §12A.9 defaults (`origin/HEAD` first); §6.5 |
| affected-base-default-conflict | Superseded by affected-default-base-contradiction (the `@{u}` default would compare a pushed branch only with itself) |
| lint-main-fails-own-docs, no-main-lint-self-fails | §6.5 branch-name regex plus the `lint:allow-main` marker |
| actions-cache-artifact-underspecified, actions-fork-contract-and-naming, actions-repo-naming | §12.7 store contract and one action set; §3.3; §3.6; §7 HTTP; §0; §12.3; §14.3; §18; §19 decision 5 |
| ci-later-steps-asc-home-and-runner-mounts | §3.5 `$ASC_HOME` from the gitfile; §12.3 step 6; §12.7 runner mounts; §15.1 `ASC_TOKEN` |
| historyapi-impl-split | §4.3 `HistoryApi`, `BlameApi`, `ProjectsApi` |
| bucket-local-refs-two-clones | §3.4 single-writer scope and message; §18 phase 2 exit |
| ascignore-shadows-gitignore, migrate-ignore-default-and-stale-text, asc-info-exclude-location | §2.4.2; §9.3; §10.4; §10.5 |
| lfs-filter-add-fails | §2.4.1 |
| hooks-must-vs-list | §9.4.1 `reference-transaction`, never-run list |
| server-whole-file-buffer-cap | §16.4 `limits.text_memory` |
| fork-dedup-deviation-unsigned, fork-dedup-deviation-unrecorded | §19 decision 6; §0; §1; §3.3 |
| gittreeoverride-value-format, git-tree-override-format | §3.1, §2.5, §8.4 (body without header) |
| client-raw-git-extras | §8.4; §6.3 push |
| import-non-commit-tags | §14.1 step 6 |
| manifests-eager-terminology | §5 table and bullet; §12A.5 |

**Round-3 audit fixes (v1.3)**

| Gap | Fix |
|---|---|
| stacks-design-not-folded, stacks-design-exists-not-folded | §20 (stacks.md D1–D12 normative); §2.2 kind 6, §3.1 `m/`, §7 `OBJ_KIND_CHANGE_META = 9`; §3.6 `change_versions` roots; §5 `diffstat.v1`; §8.2, §9.2 tables; §11.1 `asc.stack.*`; §11.2 stack verbs; §11.5 no new git flag; §12.6 stack queue; §13.3 C34–C48; §13.5 topic `asc-<change>`; §16.3; §17.1; §18 phases 1c/4s/9s; §19 pending note removed. PLAN unit ids are C01–C52 (S-ids collide with spikes) |
| shim-asc-verbs-shadow-aliases-and-git-dash-commands (shim-verb-precedence) | §11.4 resolution order per argv[0]; §11.1 `git-<name>`/`asc-<name>` external commands; §9.4.1 child PATH; §12A.9; §17.1 `external-commands.t` |
| turbo-index-reflects-head-breaks-staged-contract | §9.4 index = HEAD overlaid with set 0, updated on staging commands; §12.1 |
| no-main-lint-misses-common-forms | §6.5 regex and scan set; `-c init.defaultBranch=master` for real-git scripts |
| no-raw-git-lint-fails-built-code | §11.4 `lint:allow-raw-git` for test fixtures below `asc-git` |
| unsafe-forbid-and-allowlist | §15.2 `unsafe_code = "deny"` plus a module allow-list |
| proto-fails-buf-standard-lint | §7 buf lint exceptions |
| local-ref-update-chokepoint | §9.4.1 one local ref-update function |
| commit-hook-failure-rollback | §9.7 step 0 snapshot and restore of set 0 |
| ci-tier-stack-config-undefined-unowned | §3.5 `asc.cache.dir`/`asc.cache.readOnly`; §12.3 step 6 common config |
| plan-job-rules-file-without-checkout | §12.2 `--rules` read at the merge base; `--rules-file` |
| bucket-local-refs-storage-undefined | §3.4 dedicated `repos` row per bucket |
| workspace-deps-rule-blocks-later-units, status-stale-u11-u12-u14-built, cli-unit-acceptance-uses-sibling-commands, asc-serve-no-handler-or-route-extension-points, worktree-convert-plain-unowned | PLAN only (rules, briefs, CARRY.md) |
| roadmap-and-stale-text | §18 (M-meta week ≈ 9 without `FindLanding`; phases 4 and 6), Appendix A crate count, §10.4/§10.5, §9.2 `display_ids` removed |
| turbo-staged-file-contract | §17 turbo row |
| index-renderer-ownership, u01-v11-delta-unowned, unowned-cli-commands | PLAN only (U16 index writer, U01b, U24f) |

**Round-4 audit fixes (v1.4)** (changelog: v1.4 closes the major contract gaps of the round-4 audit; the blocker entries are folded in by a later pass)

| Gap | Fix |
|---|---|
| rewrite-sink-contract-insufficient-and-double-owned | §20.3 sink contract (`RewriteSink::record`, `OpId`, the keep/mint input table, `snapshot_wc` before `merge --abort` and `rebase --abort`); the owning units make their own sink calls (PLAN) |
| engine-local-ref-moves-bypass-chokepoint | §9.4.1 engine code never moves a local ref; CLI engine over `ReadOnlyRefs<SqlRefs>` |
| staging-overlay-breaks-on-head-moves | §9.5 HEAD moves keep the staging (`StagingAfterMove`) |
| module-registration-exception-too-narrow | PLAN only (general exception covers any `mod.rs` and parent module files) |
| stack-server-and-sync-hooks-unowned | §20.4 server writes meta for every changeset it creates; hooks `IngestObserver`, `LogEntryEnricher`, `SyncExt`, `Insights` group (PLAN) |
| stack-proto-messages-undefined | §20.5 full `repo.proto` and `workspace.proto` definitions; `ReviewState` moved to `repo.proto`; `UpdateRefsRequest.no_content_merge` |
| object-stack-selector-single-owner-blocks-remote-tier | §3.5 `Ctx` builds the tier stack; remote tier by remote kind; read-only tiers never write or delete |

**Round-5 fold (v1.5)** (changelog: v1.5 folds `docs/research/branchless.md` into §21 and the sections it amends)

| Gap | Fix |
|---|---|
| branchless-by-default-missing-and-contradicted, checklist-branchless-by-default-missing | New §21 (BD1–BD12), §0 decision 13, §1 and §1.1 rows; the five contradicting rules replaced: §0 "write-behind" (out of scope), §6.1 branch scope and drafts, §6.3 "nothing is uploaded before push", §9.1 daemon "does not upload", §15.2 secrets; §2.7 ref scope and §6.1 pushed developer branches replaced by user refs |
| branchless remote effect vs stacks D4 | §20.1 D4 recorded amendment (local commands unchanged; remote effect of `push` and the cloud pull of `fetch` depend on the mode) |
| branchless vs MCP tool naming and write gate | §21.7: tools are `drafts`, `cloud_status`, `hide`, `unhide` (short names; no separate push tool, `submit` covers it); `hide` and `unhide` need `write=remote` when the workspace uploads, as `create` and `modify` do |
| C01 is built; the migration-mode id tier | §21.6.2 and PLAN B18a: a new file and unit, C01 unchanged |
| spec edits of branchless §8 | §3.5, §3.6, §6.3, §6.4, §7, §8.2, §8.3, §8.8, §9.1, §9.2, §11.1–§11.3, §13.3, §15.2, §16.3, §17.1, §18, §19 (decision 7, spike S13), §20.2, §20.3, §20.7 |

**Round-6 fold (v1.6)** (changelog: v1.6 folds `docs/research/mcp.md` into §22 and the sections it amends)

| Gap | Fix |
|---|---|
| mcp-built-in-missing, checklist-mcp-not-folded | New §22 (M-D1–M-D10; surfaces, tools, schemas and codec, safety, efficiency, crate layout, auth, configuration), §0 decision 14, §1 row; PLAN §M units M01a–M28 |
| MCP versus stacks D12 ("AI features stay out of core") and stacks.md row 114 ("MCP server: OUT") | §20.1 D12 now makes agent interfaces core and keeps AI review and chat out; stacks.md row 114 is superseded by §22 |
| `--json` "proto3 JSON" (§11.2) versus snake_case, hex-id shapes (§20.7) | The asc JSON profile `json.profile.v1` (§22.4.2) for every `--json` output; `asc api` keeps standard proto3 JSON (§7, §11.2); §20.7 shapes gain `tz`, `status: ambiguous`, `candidates`, and the `{"ops": [...]}` and `{"versions": [...]}` wrappers |
| server `Commit` writes no `ChangeMeta` | §8.7 bullet and unit M18 (through the §20.4 `IngestObserver`) |
| `cli/json.rs` owner named as U21a/U73 in mcp.md | U20 owns it; M01c makes the print-through-the-profile hunk (PLAN §M amendments) |
| undo/redo cannot be limited to the agent's own operation | §20.7 `--op <id>` with `STALE_OP` |
| spec edits of mcp.md §9 | §7, §8.1, §8.7, §11.2, §13.3 (M20–M28), §15.1, §16.3, §16.4, §18 (phases 4m, 5m, 9m), §20.1, §20.7, §21.7 |
| codeintel.md §10 request for the `search` tool | §22.3.2 `search` row and note: additive fields `change`, `stack`, `diff_only`, `op`, `k`, `context_lines`, `max_tokens`, `rerank`, `include_excluded`; PLAN M15 (depends on I12) |

**Round-7 fold (v1.7)** (changelog: v1.7 folds `docs/research/codeintel.md` into §23 and the sections it amends)

| Gap | Fix |
|---|---|
| agentic-code-intelligence-missing, checklist-code-intelligence-missing | New §23 (CI-D1–CI-D8; architecture, queries, interfaces, indexing, components, security, Forgejo, benchmarks, configuration, rulings), §0 decision 15, §1 row; PLAN §I units I01–I31 (the audit's X01–X09 proposal is superseded, §23.12) |
| codeintel.md "proto3 JSON" `--json` versus the asc JSON profile (§22.4.2) | §23.4.2 and §23.12 ruling 3: the profile is the one `--json` codec; I12 depends on M01a, I17 on M01c |
| codeintel.md I09 (keep or bypass `onig`) | §23.6 and §23.12 ruling 1: keep; bypass only if the Windows build fails |
| `Fork`/`CopyClosure` copy only "the reachable closure" (§3.3) | §3.3 states the content closure only, never derived index data; test in I15 |
| spec edits of codeintel.md §10 | §3.3, §5, §7, §8.1 (`[codeintel]`, `--role indexer`), §8.2 (`ci_*`), §9.1, §11.2, §13.3 (I23–I26), §15.2, §15.5, §16.1, §16.3 (`codeintel` class), §16.4, §18 (phases 4c, 4c-MCP, 5c, 9c), §22.3.2 pointers |

**Round-8 fold (v1.8)** (changelog: v1.8 folds `docs/research/ide-plugins.md` into §24 and the sections it amends)

| Gap | Fix |
|---|---|
| ide-plugins-unspecified-and-contradicted, checklist-ide-plugins-missing | New §24 (IP-D1–IP-D15; layout, bridge protocol, feature matrix, VS Code, JetBrains, auth and offline, testing, units and API additions, risks, rulings), §0 decision 16, §1 rows; PLAN §P units P01a–P58 (the audit's IP01–IP08 proposal is superseded, §24.10) |
| §20 fixed rules and stacks.md: "there is no IDE extension" | §20 and stacks.md now say that GitLens/GitKraken-style features are data APIs rendered by ASC's IDE plugins (§24) and the Forgejo fork; the stacks.md GitKraken row keeps its OUT status for GitKraken desktop features only |
| "No IDE support" wording (v1.1 decision (3), principle 6, §0 out-of-scope, §1, §21 fixed rules) | The IDEs' built-in git is not supported and ASC-built plugins are; the full `.git` projection stays out of scope. The `[audit: user decision No-IDE]` tags stay as history |
| research A1 `behind_trunk` versus §20.6 `WsStack.behind` | No new field; `behind` is defined as the capped count of trunk commits not in the stack base (§24.8.2 A1) |
| research allowlist `stack get --worktree` as a boolean versus §20.7 `--worktree <path>` | The allowlist types it as a path (§24.10 ruling 4) |
| research A4 `targets[].argv` with no source in §12A | An optional per-project `argv` table in `.asc/projects.toml`; asc never runs it (§12A.8, §24.8.2) |
| IP-D7 `create --no-branch` versus §20.7 (implied only in branchless mode) | The plugin passes `--no-branch` only when `Hello` reports branchless mode (§24.10 ruling 6) |
| spec edits of ide-plugins.md §9 | §7 (`bridge.proto`, `Workspace.ReadFile`), §9.1, §11.2 (`asc api --stdio`, `reorder --todo`, `split --hunks`, the P45 `--json` forms, `ASC_NO_ALIASES`), §12A.8, §13.3 (P04–P07, P58), §16.4, §17, §18 (phases 5p, 9p, 10, 10j), §19, §20, §20.6, §21, §22 |
