# PLAN status audit

Audited 2026-10-25 against `docs/PLAN.md` (master). Evidence is the oldest commit naming the unit, or a file path. Excluded: K* units, P04-P07 and P01a-P44b. Forgejo-side units are checked in `D:/dev/forgejo` and `D:/dev/forgejo-runner`. "Implemented" means code landed; acceptance commands were not re-run.

Not complete: C37, C41b, C44, C47, I24, P58

| Unit | Title | Status | Evidence |
|---|---|---|---|
| U01 | Workspace skeleton (built, v1.0) | implemented | c58b1e1 |
| U01b | Skeleton delta for v1.1/v1.2 | implemented | b22be9b |
| U02 | Typed ids | implemented | f722da5 |
| U03 | Names and paths | implemented | 8229831 |
| U16a | RealGit resolution | implemented | 25480e0 |
| U04 | Objects and canonical codec | implemented | 3396c78 |
| U05 | Git object rendering | implemented | 3ffb4bb |
| U06 | Content ingest and chunking | implemented | e306818 |
| G01 | Graph model, canon, patterns, selectors, config | implemented | 4b8735d |
| U07 | Objects trait, keys, bucket (memory/fs) | implemented | a1cd5e4 |
| U09 | Refs trait and SqlRefs | implemented | 2b865ed |
| U41 | Proto and conversions | implemented | eb7d0f6 |
| U08 | CacheDir, StoreSet, Tiered | implemented | 735cbc5 |
| U16 | Git index file and ODB access | implemented | 9b7cb37 |
| U29 | S3 bucket objects and put fallback | implemented | 807fc65 |
| G02 | Ownership, linking, affected algebra | implemented | 9df83ce |
| U10 | Tree apply/diff | implemented | 6c1cba3 |
| G03 | Node detector and bun.lock | implemented | 9921e76 |
| G04 | Rust detector and Cargo.lock | implemented | efa78a3 |
| G05 | Python detector, uv.lock, requirements | implemented | 9166c0b |
| G06 | .NET detector | implemented | a442f76 |
| U11 | Changeset build and validation | implemented | 0fd34d3 |
| U15 | Line diff and output formats | implemented | e415da0 |
| U12 | Commit graph and log | implemented | d3cafe6 |
| U14 | Derived data (gen, gitcommit, DerivedKeyed) | implemented | 1984476 |
| U13 | Revspec resolution | implemented | 751fa4a |
| U14b | Skip lists | implemented | e1ced15 |
| U17 | $ASC_HOME and state.db (`asc-state`) | implemented | 67d19a8 |
| U28a | Blame engine | implemented | 67f2566 |
| U31 | Sync engine (push/fetch) and `HistoryApi` | implemented | 5ccaca5 |
| U33 | Git import | implemented | 8a1432e |
| U34 | Git export | implemented | 741fabd |
| G07 | Project graph engine integration | implemented | 4201af9 |
| U18 | Plain working copy | implemented | 2201faf |
| U19 | Synthetic `.git` layout and config | implemented | dcbe222 |
| U26 | Merge, rebase, cherry-pick, land check | implemented | cfb70d1 |
| U35 | ASC round-trip milestone | implemented | 23b99e7 |
| U20 | CLI frame, init, config, native table, messages, env | implemented | d7be1de |
| U42 | gRPC server/client, auth interceptor | implemented | 6b04791 |
| U20a | git shim | implemented | 1623f7c |
| U21a | CLI status | implemented | 2a929b6 |
| U21b | CLI add/rm/mv | implemented | 3c9559f |
| U21d | CLI restore/reset | implemented | ce89e92 |
| U22 | CLI commit and hooks runner | implemented | 6e682be |
| U23 | CLI branch/switch/checkout/tag | implemented | 700e78b |
| U24a | CLI log/show (incl. `--graph`) | implemented | c877bd2 |
| U24c | CLI diff/diff-tree/diff-index | implemented | 38ee22d |
| U24d | CLI rev plumbing | implemented | 8ee83f2 |
| U24e | CLI object and ref plumbing | implemented | 60a72e7 |
| U36 | Synthetic index export for turbo | implemented | 7725357 |
| U43a | Server config, schema, registry | implemented | 3765edb |
| U52 | IPC transport and Workspace service | implemented | 600cb59 |
| U21c | CLI `add -p` | implemented | faf1ca4 |
| U43b | Server ingest and UpdateRefs | implemented | d26b500 |
| U25 | Compatibility suite, local-only milestone | implemented | 87fe48a |
| U43c | Server outbox and HTTP | implemented | ecd3eff |
| U65 | Mainline and path index | implemented | a6a8aa5 |
| O01 | GC (two-mark, fenced) | implemented | e7afd8c |
| U24b | CLI ignore/attr/grep/clean | implemented | f74afab |
| U25a | Tool trace corpus | implemented | 12d3f99 |
| U27a | CLI merge/cherry-pick/revert | implemented | d4209ff |
| U28b | CLI blame | implemented | 54a7acf |
| U37 | Hook and tool scenarios | implemented | 9ac95c4 |
| U54w | `asc worktree` (plain) and Claude Code hooks | implemented | 0bf4e78 |
| U54v | Journal, virtual status, switch intent, times | implemented | d383dd2 |
| G08 | CLI `affected`/`projects`/`graph` (local) | implemented | e23da79 |
| U44 | Forgejo client (authz, hooks) | implemented | a0e0a52 |
| U47 | Decoded read RPCs | implemented | 5f3ea53 |
| U49 | Turbo /v8 cache | implemented | 76a838f |
| U69 | Admission control, retry, coalescing | implemented | fa90b05 |
| U72 | Seed snapshots | implemented | 99ffbd4 |
| U27b | Patches and bisect | implemented | 85c102e |
| U27c | CLI rebase (non-interactive) | implemented | 31ca6b5 |
| U27e | CLI stash | implemented | c2c12ec |
| U55 | VFS provider core | implemented | d13f58b |
| U45 | Mirror mode | implemented | a0dcc05 |
| U46 | Land | implemented | 42f7372 |
| U67 | Classify | implemented | 5c31ef6 |
| U70 | Cache role (`asc serve --role cache`) | implemented | 7403712 |
| G09 | Projects service | implemented | a6a8041 |
| G11 | ASC affected validation | implemented | 38a94f2 |
| U27d | CLI `rebase -i` | implemented | 79d8f08 |
| U32 | CLI remote/fetch/push/pull/clone (bucket remotes) | implemented | 6db559f |
| U48a | Commit, Ingest and Apply RPCs | implemented | 5142eeb |
| U53a | Daemon lifecycle | implemented | 57d0eb9 |
| U56 | ProjFS binding and callbacks | implemented | 747879c |
| U59 | FUSE provider | implemented | 2badc9a |
| U60 | Submodule read-only composition | implemented | bad2a23 |
| U39 | Git-remote mode | implemented | b4b9d26 |
| U24f | CLI remaining plumbing and history verbs | implemented | 385753a |
| U48b | Merge styles and dry run | implemented | 0ef23fd |
| U48c | CopyClosure and RepoAdmin | implemented | 52761de |
| U48d | ImportGitPush and expiry | implemented | 5986608 |
| U53b | Daemon service and scheduler | implemented | 6385cb6 |
| U53c | Daemon OS service install | implemented | ef54aa0 |
| U40 | Doctor and reclaimable report | implemented | ab6ef37 |
| U50 | CLI server wiring | implemented | a47a09d |
| U57 | ProjFS switch, tombstones, reconcile, dehydration | implemented | 38d726e |
| U66 | Landing map and FindLanding | implemented | 4e52615 |
| U68 | Rebase RPC and land content merge | implemented | 7f257e7 |
| U73 | Plugin API (Workspace plugin RPCs) and `asc watch` | implemented | 0c497eb |
| U40b | Operational verbs | implemented | c5b77f5 |
| U51 | Deploy bundle and smoke | implemented | 34a9f6b |
| U58 | Windows virtual end-to-end | implemented | e011de7 |
| U61p | Turbo contract suite (plain) | implemented | a3e916c |
| U62 | asc ci checkout | implemented | 74dbdb9 |
| U71 | Metadata-first milestone (M-meta) | implemented | 7909ddd |
| G10 | CLI affected: server path and `--repo` | implemented | 87f9cdd |
| U61v | Turbo contract suite (virtual) | implemented | 0b1b021 |
| U63 | asc ci changed/affected/warm | implemented | 2611068 |
| U75 | asc ci cache/artifact (Actions fork backend) | implemented | 57926ac |
| U76 | `WatchRefs` ref-update stream | implemented | f381349 |
| U77 | Client credential resolver, git-compatible (helpers, askpass, netrc, `http.extraHeader`) | implemented | 71d6ac1 |
| U78 | Server credential edge: Basic, Bearer and `token` parsing, `WWW-Authenticate` challenge | implemented | b2a782a |
| U79 | Auth compatibility suite against real git (credential helpers, redirects, the asc edge) | implemented | 759514c |
| U80 | Git compatibility module: feature `git-compat`, `ServerGit`, the git view (SPEC §25) | implemented | 6317f46 |
| U81 | Git upload-pack: smart-HTTP `info/refs` and `git-upload-pack`, the disabled stub | implemented | 9ac1116 |
| U82 | Git receive-pack: push through the pre-receive hook into `UpdateRefs` | implemented | 7ebf125 |
| U83 | Git client compatibility suite and the `--no-default-features` build | implemented | b1aa953 |
| U84 | SSH public keys in the `Authorizer` and the token store (`ssh_keys`, `asc serve ssh-key`) | implemented | db89318 |
| U85 | Forge SSH key lookup: `InternalAuthz` identifies and authorises Forgejo user and deploy ke | implemented | 1245c0d |
| U86 | SSH listener and key authentication (`russh`, feature `git-ssh`, host keys, channel policy | implemented | 453b2b5 |
| U87 | Git over SSH: `git-upload-pack` and `git-receive-pack` on the SSH channel | implemented | 3c9b37f, crates/asc-git/src/compat/server_git.rs |
| U88 | SSH compatibility suite with real git and OpenSSH, and the no-weakening check | implemented | 93657ef |
| U89 | asc transport policy: https or ssh only, plain http refused everywhere unless the loopback  | implemented | 384662d |
| U89b | HTTP/3: opportunistic h3 in the asc HTTPS client (on `Alt-Svc: h3=…`, fall back to h2 on an | implemented | 60b6cfe |
| U64 | Actions, ASC Actions fork and ASC workflows | implemented | f9b1b21 |
| U30 | BucketRefs and probe (deferred, off the critical path) | implemented | bf4e65b |
| U38 | *(removed: `git-remote-asc` is not built, SPEC §9.6)* | withdrawn | removed in PLAN (git-remote-asc not built) |
| C01 | `ChangeId`, content-addressed `ChangeMeta`, display, import derivation | implemented | 2576174 |
| C02 | Local change index and rewrite recording | implemented | b44397d |
| C03 | Operation log store, pruning, argv redaction | implemented | fd1fa30 |
| C04 | `RewriteSink`/`OpSink` implementations for git commands (ids, op log, pre-destroy snapshot | implemented | 85afd8d |
| C05 | Stack model and revset subset (`-r/--revset` only) | implemented | de52fad |
| C06 | In-memory restack planner and executor (with re-signing) | implemented | b86e959 |
| C07 | Stack CLI frame, the D5 precedence hook-up, `--json` types | implemented | b6640ae |
| C08 | `asc sl`, `asc stack info\/parent\/children\/trunk`, `asc change-id` | implemented | 919325b |
| C09 | Navigation `up/down/top/bottom/next/prev` (exit 2 at a fork) | implemented | 7f0e06a |
| C10 | `asc restack`, `asc continue`, `asc abort` | implemented | 7badaa4 |
| C11 | `asc create`, `asc modify`, `asc reword` (hooks) | implemented | 008fab2 |
| C12 | `asc absorb` | implemented | 13344c9 |
| C13 | `asc split`, `asc squash`, `asc fold` | implemented | 49e8152 |
| C14 | `asc move`, `asc reorder`, `asc stack rename\/delete\/pop` | implemented | 614e7fd |
| C15 | `asc undo/redo/op` | implemented | 7fa68d2 |
| C16 | *(withdrawn: opt-in `restackOnRewrite`, SPEC §20.1 D4)* | withdrawn | opt-in restackOnRewrite dropped |
| C17 | Proto: `changes.proto` and conversions, plus the `repo.proto` amendment | implemented | fc3d5b3 |
| C17x | `LogEntry` to `history::LogMeta` conversions (follow-up to C17 and U31) | implemented | 76a0be3 |
| C18 | Server change index, `m/` ingest and validation, versions, GC roots | implemented | 3509ba3 |
| C19 | `ResolveChanges`, `Stack`, `Versions`, `SetReviewState` | implemented | a14df31 |
| C20 | `InterDiff` | implemented | 2245c9b |
| C21 | `Mergeability` (one row per head), `Overlaps`, `open_paths` | implemented | crates/asc-serve/src/changes/{mergeable,overlaps}.rs, migration 0202_open_paths (no unit-named commit) |
| C22 | `RestackChanges` (actor committer, one timestamp per call) | implemented | 20acdb5 |
| C23 | Client transport of change metadata (push closure `m/`, fetch `LogEntry.change/meta`) | implemented | ff9a30b |
| C24 | `asc submit` | implemented | 1b4f3e4 |
| C25 | `asc sync` (with the Stage 0 fallback) | implemented | 6ea2230 |
| C26 | `asc stack get\/freeze\/unfreeze` (`--worktree` opens a plain worktree) | implemented | e45e7b7 |
| C27 | `asc evolog`, `asc interdiff` | implemented | 17fa92b |
| C28 | `asc stack merge` (calls `Changes.RequestMerge`) | implemented | de7f7bc |
| C29 | `CommitGraph` RPC with capped lanes, `diffstat.v1` | implemented | ed8b035 |
| C30a | `LineHistory` RPC with scan budget | implemented | a791ba8 |
| C30b | `Compare` RPC | implemented | 53b7e52 |
| C30c | Pickaxe `Log` (path required, scan budget) | implemented | c1a5c57 |
| C30d | `RepoConfig` RPC (autolinks, `review.toml` at a rev) | implemented | 86fc1e4 |
| C31 | Workspace plugin additions (SPEC §20.6) | implemented | c89e649 |
| C32 | `asc share` (rooted by `refs/snapshots/<id>` with expiry) | implemented | b745fa9 |
| C33 | `asc stack exec` (v1.x) | implemented | 5a8a16d |
| C34 | PR ↔ change, `stack_parent_id`, the `merge_base` rule and its recomputation patch, AGit lo | implemented | forgejo repo c348fc7221 |
| C35 | `SetReviewState` notifier on PR, review and check events | implemented | forgejo repo 6acd5c6032 |
| C36 | Stack panel, needs-restack banner, mid-stack merge block (web and `POST /pulls/{n}/merge`) | implemented | forgejo repo acf55a84f5 |
| C37 | Versions dropdown, interdiff, since-last-review | partial | forgejo: services/pull/asc_versions.go and templates/repo/pulls/asc_versions.tmpl exist but are uncommitted (in progress) |
| C38 | Merge stack (N), post-merge `RestackChanges`, approvals kept | implemented | forgejo repo d23c205806 |
| C39 | Stack queue: merge-when-ready cascade, order, pause, revalidate, the `RequestMerge` intern | implemented | forgejo repo 3593f94ca6 |
| C40 | Actions stack context (K14 extension) | implemented | forgejo repo 5c5537f9b0 |
| C41a | Inbox state model (whose turn, pin, snooze) | implemented | forgejo repo bcee3ebea4 |
| C41b | Inbox API `GET /api/v1/user/asc/inbox` | missing | forgejo: no /api/v1/user/asc/inbox route (C41c page committed without it) |
| C41c | Inbox page | implemented | forgejo repo f8dd7aca7a |
| C42 | Commit graph page over `CommitGraph` | implemented | forgejo repo c463bba90a |
| C43 | Overlap and mergeability badges, stack compare view | implemented | forgejo repo 71021be626 |
| C44 | Suggested changesets (v1.x) | missing | forgejo: no suggested-changesets code (v1.x) |
| C45 | Git import writes `ChangeMeta` (jj header, Gerrit trailer, scoped id) | implemented | dc771a1 |
| C46 | Branch-chain PR retarget (SPEC §20.5 flow 6) | implemented | forgejo repo e8b7c43b6e |
| C47 | "Restore version" (moves `refs/pull/<n>/head` to a kept version) | missing | forgejo: no Restore version code |
| C48 | "Revert landed stack" (`Apply(REVERT)` over `FindLanding`, newest first) | implemented | forgejo repo 590f949e64 |
| C49 | `Watch KIND_STACK` events (server emit, client delivery) | implemented | crates/asc/src/daemon/watch_stack.rs, tests/stack_watch.rs (no unit-named commit) |
| C50 | `RequestMerge` RPC (proxies to the forge's queue) | implemented | crates/asc-serve/src/changes/request_merge.rs (no unit-named commit) |
| C51 | `asc uncommit`, `asc unamend`, `asc abandon` | implemented | 73ebc9c |
| C52 | Server `no_content_merge` land flag | implemented | 3d38f31 |
| B01 | Config keys and effective-mode resolver | implemented | 19586d3 |
| B02 | Local `draft_heads`, `cloud_queue`, `cloud_state`, `cloud_base` tables | implemented | c72297a |
| B03 | `DraftHeadSink`: head and queue maintenance | implemented | 4b5fd56 |
| B04 | Visible set, hide and unhide rules, obsolete heads | implemented | 7ce6821 |
| B05 | `asc hide`, `asc unhide` | implemented | 140a88d |
| B06 | Local GC of hidden un-uploaded drafts | implemented | 6020ae7 |
| B07 | Proto `cloud.proto` and the `repo.proto` follow-up | implemented | 3d8a369 |
| B08a | Server ref policy, user-ref classification, advertisement, `ImportGitPush` mapping | implemented | 7041ac4 |
| B08b | User-ref authz, per-user ref cap, quota on user-ref pushes | implemented | 83786fc |
| B08c | User-ref TTL, `landed_at` marker and expiry | implemented | d5e3d1a |
| B09 | Cloud workspace tables and RPCs, caps, label validation, `quota::charge` | implemented | 2f4ee6e |
| B10 | Expiry worker, GC roots, `Purge`, `cache_evict` | implemented | crates/asc-serve/src/cloud/{expiry,purge}.rs (landed in 6df5761, no unit-named commit) |
| B11 | Cloud sync engine (three-way merge, CAS retry) | implemented | d70ffbd |
| B12 | Client push mapping (user ref plus per-change AGit) | implemented | 8a21d28 |
| B13 | Upload pipeline (closure, `HasObjects`, `PutObjects`, `Ingest`), bounded | implemented | 422cf5a |
| B14 | Daemon uploader and scheduler task | implemented | 8a38549 |
| B15 | Cloud pull on fetch and `Watch KIND_CLOUD` | implemented | 3cb5891 |
| B16 | `asc cloud status\/sync\/join\/list\/release\/purge`, `asc sl --cloud --user` | implemented | 518a841 |
| B17 | Forgejo: branchless repo setting, no branch rows or push events for user refs, draft PR de | implemented | forgejo repo cffef09910 |
| B18 | `asc serve admin branchless enable` | implemented | 7dccafc |
| B18a | Migration-mode change-id tier (follow-up to built C01) | implemented | aad4145 |
| B19 | `asc migrate branchless` | implemented | df8c1ef |
| B20 | Withdrawn (folded into C01 and then into B18a) | withdrawn | folded into C01/B18a |
| B21 | Compat suite in both modes; `asc-sim` cloud scenario | implemented | bdb7e2c |
| B22 | MCP handlers for `drafts`, `cloud_status`, `hide`, `unhide` | implemented | 1cb2772 |
| B23 | Forgejo "Your drafts" page (v1.x) | implemented | forgejo repo 41a4a08d40 |
| B24 | `asc doctor --workflows` | implemented | fe4cc4b |
| B25 | Benchmark scenarios for the commit cloud | implemented | 310694b |
| B26 | `legacy_shared` conversion | implemented | cae51c4 |
| B27 | CI: draft-PR trigger spike and stacked-PR base for `asc ci checkout` | implemented | 63eedf9 |
| B28 | Pre-upload guard (ignored and denied paths, bounded secret scan) | implemented | 40528a2 |
| B29 | Live restack and update executor | implemented | 14e8ae6 |
| B30 | Daemon live subscription, debounce and poll fallback | implemented | 9e40a9b |
| B31 | `asc live` verbs (pause, resume, status) | implemented | 909f7b2 |
| M01a | Descriptor set export, `options.proto`, the asc JSON profile codec | implemented | a651491 |
| M01b | Annotate the existing protos with `(asc.v1.json)` | implemented | 8c29c5f |
| M01c | `--json` prints through the profile | implemented | 1b43b4c |
| M02 | `cli.proto` and the reconciled `--json` shapes (op log wrapper, signatures) | implemented | 76dfb4a |
| M03 | asc-mcp skeleton, `tools.toml`, schema generator, catalogue, `tools/list` snapshot | implemented | bfbad9e |
| M04a | Text renderers moved into the engine (`asc-engine::render`) | implemented | 878505d |
| M04 | Shaping: stream folding, token budget, truncation, cursors | implemented | 8a14ff9 |
| M17 | Generated tool reference `docs/mcp.md` (drift-checked) | implemented | eb9625e |
| M05 | Handler core: discover, tools/call, error mapping, gating | implemented | cd9f24f |
| M06 | MRTR elicitation and sealed `requestState` | implemented | 473dc57 |
| M07 | Resources, templates, completion, prompts | implemented | 749e779 |
| M08 | `RpcBackend` over `RpcClient` and the IPC `WorkspaceClient` | implemented | 7a9fb36 |
| M12a | `asc serve /mcp` transport (stateless HTTP, Origin and header checks, caps) | implemented | 95213c0 |
| M09 | `asc mcp` over stdio, `LocalBackend`, path gate, stdout guard | implemented | 7570b85 |
| M12b | `ServeBackend` and rate classes | implemented | 1f0e6fb |
| M15 | `search` tool (listed when `search.v1` is advertised) | implemented | 12cc101 |
| M09b | `asc mcp install` | implemented | 4e3d0bb |
| M10 | Local write tools through child verbs | implemented | d89067b |
| M11 | Local `subscriptions/listen` over `Workspace.Watch` | implemented | b5c2eaf |
| M13 | Server auth: PRM, 401/403, scope-filtered tools over the git authorizer | implemented | 2fd552c |
| M29 | MCP authenticator on the git `Authorizer` (per-repo scopes, audit; credential parsing is U | implemented | 163348b |
| M30 | Standalone token store `Authorizer` and `asc serve token` | implemented | 56c5241 |
| M31 | `asc mcp --remote <url>` (git credential helper, HTTP proxy) | implemented | c2a7c6c |
| M32 | Server `land` and `classify` tools | implemented | 1f3b3d0 |
| M33 | Optional OAuth 2.1/OIDC: PRM issuers, token validation through the authorizer | implemented | 23bce16 |
| M34 | (v1.x) Optional `forge` toolset behind a `ForgeAdapter` | implemented | 938a595 |
| M18 | Server `Commit` writes a `ChangeMeta` | implemented | a568bf9 |
| M14 | Server write tools: `commit`, `submit`, `rebase`, `restack` | implemented | 6b79328 |
| M16 | MCP budgets and conformance (RSS, latency, tokens, inspector job) | implemented | 66e684a |
| M20 | SUPERSEDED (SPEC §22.9, v1.10): Forgejo MCP framework (`/api/mcp`, auth reuse, read-only g | withdrawn | superseded by SPEC 22.9 v1.10 (forge MCP is M34) |
| M21 | SUPERSEDED (SPEC §22.9, v1.10): Forgejo `pr_read` with `AscHandles` and the stack panel | withdrawn | superseded by SPEC 22.9 v1.10 (forge MCP is M34) |
| M22 | SUPERSEDED (SPEC §22.9, v1.10): Forgejo `pr_write` and `review_write` | withdrawn | superseded by SPEC 22.9 v1.10 (forge MCP is M34) |
| M23 | SUPERSEDED (SPEC §22.9, v1.10): Forgejo `issue_read`, `issue_write` | withdrawn | superseded by SPEC 22.9 v1.10 (forge MCP is M34) |
| M24 | SUPERSEDED (SPEC §22.9, v1.10): Forgejo `actions_read`, `actions_write` | withdrawn | superseded by SPEC 22.9 v1.10 (forge MCP is M34) |
| M25 | SUPERSEDED (SPEC §22.9, v1.10): Forgejo `inbox_read`, `inbox_write` | withdrawn | superseded by SPEC 22.9 v1.10 (forge MCP is M34) |
| M26 | SUPERSEDED (SPEC §22.9, v1.10): Forgejo OAuth: resource indicators, audience, `asc-mcp` cli | withdrawn | superseded by SPEC 22.9 v1.10 (forge MCP is M34) |
| M27 | SUPERSEDED (SPEC §22.9, v1.10): (v1.x) CIMD client registration | withdrawn | superseded by SPEC 22.9 v1.10 (forge MCP is M34) |
| M28 | SUPERSEDED (SPEC §22.9, v1.10): Forgejo `lockdown` mode | withdrawn | superseded by SPEC 22.9 v1.10 (forge MCP is M34) |
| I01 | Doc model: ords, language set per path, byte skip rules, `.asc/codeintel.toml`, secret scan | implemented | 91ae0a4 |
| I02 | Trigram extraction and regex-to-trigram planner (codesearch port) | implemented | 8347c98 |
| I03 | Tree-sitter tags: six grammars, project-owned `tags.scm`, containers, outline | implemented | 090f3cd |
| I04 | AST chunker (split, merge, collapsed signatures) | implemented | 814fcc7 |
| I05 | Scope algebra: `tdocs.v1` with `dups` counts, `xdocs` (attributes with `.ascattributes` fir | implemented | 71ef5e5 |
| I06 | tantivy schema (`files`, `chunks`), segment writer and reader, bucket-persisted segments,  | implemented | 5df3a6a |
| I07 | Exact search executor: plan → postings ∧ scope → verify → locate; streaming completion und | implemented | a24b0d4 |
| I08 | Symbol queries: def, refs, callers, outline, find; import and project-graph ranking | implemented | 59d39fd |
| I09 | `asc-embed`: fastembed runtime, model registry (sha256, licence allow-list), i8 calibration | implemented | a12df19 |
| I10 | Vector segments: usearch i8 HNSW, filtered search, brute-force small scopes, b1 local form | implemented | efc9d8b |
| I11 | Hybrid ranking: chunk BM25 + vector + symbol list, RRF k = 60, optional reranker | implemented | 1de210e |
| I12 | `codeintel.proto` and conversions | implemented | eab06c7 |
| I13 | Job queue `ci_jobs`, outbox trigger, leases, priorities | implemented | 15296e7 |
| I14 | Workers: file, embed, tiered merge, sweep and scope-warm jobs; model generation flip | implemented | ca14c40 |
| I15 | Import backfill (heads only), `admin codeintel backfill`, on-demand indexing | implemented | de348ba |
| I16 | `CodeIntel` RPC handlers: authz per namespace, composed fan-out, page tokens, rate class | implemented | 4875720 |
| I17 | CLI `asc search`, `asc sym`, `asc semantic`, `asc codeintel status`, with `--json` (needs U50) | implemented | 82ceb88 |
| I18 | Daemon overlay index and workspace-scope merge; Workspace IPC methods | implemented | 94df8df |
| I19 | Local degraded index: the `asc codeintel worker` helper (limits of §1.5), `$ASC_HOME/ci`, si | implemented | 3de2558 |
| I20 | Local embedding opt-in: helper download and verify, b1/i8 local vectors | implemented | 2b70597 |
| I21 | Wire mcp.md's `search` tool (M15) to `CodeIntel`: mode mapping, additive fields, tool desc | implemented | f50216f |
| I22 | Output renderer: token budget, `also_at` dedup, redaction, control-char stripping, provena | implemented | a59c00a |
| I23 | Forgejo `asc` code-indexer backend | implemented | forgejo repo e4e5c8ccb9 |
| I24 | Repo search page: modes, scope selector, coverage note; shows server-redacted text only | missing | forgejo: no repo search page with modes, scope and coverage note |
| I25 | File-view symbol panel and outline; PR "find usages in base" | implemented | forgejo repo 62c6c15200 |
| I26 | `/explore/code` fan-out and the `/api/v1/.../asc/{search,symbols,semantic}` API | implemented | forgejo repo 27f88e84be |
| I27 | `bench/codeintel`: scenarios against rg, git grep and Zoekt; gates of §8 | implemented | f0628d9 |
| I28 | Semantic eval set from ASC history; model and quantisation bake-off | implemented | db3aab9 |
| I29 | Metrics `asc_ci_index_lag_seconds`, `asc_ci_jobs{kind,state}`, `asc_ci_query_seconds{rpc}`, ` | implemented | 64c19e8 |
| I30 | (v1.x) Index-accelerated `git grep <rev>` with byte-identical output | implemented | 4fc0ba0 |
| I31 | (v1.x) SCIP upload tier: `asc codeintel upload-scip`, documents keyed by `FileId`, precise  | implemented | e47eb02 |
| P01c | `Bridge.Token`/`StoreToken`: non-interactive credential calls, `NOT_FOUND`, `NO_CREDENTIAL | implemented | 16e9c01 |
| P35c | VS Code release workflow (`vsce` and `ovsx` from Forgejo Actions) and its dry-run checker | implemented | 3a056a4 |
| P37c | JetBrains content revisions and `DiffProvider` | implemented | 9b793a8 |
| P45 | `--json` forms and typed `OpResult`s for `restore`, `switch`, `cherry-pick`, `revert`, `cl | implemented | a327558 |
| P46 | `asc split --hunks <spec>`: non-interactive split by patch per piece | implemented | 449ae6f |
| P47a | VS Code stack commands: up/down/top/bottom with the fork quick pick, `scm/title` restack,  | implemented | c3e589d |
| P47b | VS Code cloud status item and "Show cloud drafts" toggle | implemented | b84e115 |
| P47c | VS Code graph context-menu wiring: cherry-pick and revert with host-composed confirmations | implemented | 6ad29f4 |
| P48 | JetBrains allowlist generator: Gradle task `generateAllowlist` from `allowlist.json` to `A | implemented | a72f2aa |
| P49a | VS Code hydration guard: exclusions prompt, per-process hydration notification, no-hydrati | implemented | 7c83713 |
| P49b | JetBrains hydration guard: index excludes, `.ascignore` provider wiring, notification | implemented | 313a579 |
| P50 | Spike: MCP OAuth from the VS Code and Cursor clients against `asc serve` `/mcp` (M13 PRM, M | implemented | dde1a4f |
| P51 | Spike: features lost without `VcsLogProvider`; comments API survey | implemented | 2615715 |
| P52 | Spike: Git4Idea and the synthetic `.git` root | implemented | 681628b |
| P53 | Spike: partial-line commits (`arePartialChangelistsSupported`) | implemented | b16552d |
| P54 | Spike: the `jcef` module and optional dependency across 2026.1 and 2026.2 | implemented | 5e05787 |
| P55 | Spike: OAuth loopback (moved port, `state`, PKCE, variable-port redirect against P06) | implemented | 791b803 |
| P56 | `asc-ui` split dialog (hunks and lines per piece) and submit dialog (draft/publish, reviewe | implemented | 0af79e5 |
| P57 | asc API additions A1, A2, A4: `CommitGraphResponse` first-page `total` and `ref_markers`, a | implemented | 84b3a4a |
| P58 | Inbox count: `section`, `limit`, `X-Total-Count`, `If-None-Match` | missing | forgejo: no inbox section/limit/X-Total-Count/ETag handling |
| P59 | IDE live toggle, restack item and live-conflict notice | implemented | ba07955 |
| A01 | `asc/actions` mirror | implemented | e2d2055 |
| O02 | Release and distribution | implemented | bdd40d7 |
| O03 | Migration runbook and admin verify | implemented | b3a00c3 |
| O04 | Developer docs and cheat sheet | implemented | df962c6 |
| O05 | Benchmark gate (git vs asc) | implemented | af80c81 |
| O06 | Observability catalogue | implemented | 2a37dfc |
| O07 | Fuzz targets | implemented | 5f11c84 |
| O08 | Simulation harness | implemented | 3f99492 |
| Q01 | Git-fidelity registers and lint | implemented | 14f2539 |
| Q02 | Pinned git oracle (`ASC_GIT_ORACLE`) | implemented | c8ddeff |
| Q03 | Adapter-gap classification and preflight in the compat runner | implemented | tests/compat/src/preflight.rs (landed in 6df5761, no unit-named commit) |
| Q04 | Differential fuzz against the pinned oracle | implemented | 88794fa |
| R01 | Action alias table | implemented | forgejo-runner eb9fd95 |
| R02 | Native checkout fast path | implemented | forgejo-runner 3a3de69 |
