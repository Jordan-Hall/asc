# Scion requirements checklist for asc

## Default branch is `master`, never `main` (user requirement)
All defaults, docs, examples, `asc init`, import, the Forgejo default branch, CI and tests use `master`. `init.defaultBranch` is honoured, but asc ships with `master` as its default.

## TOP PRIORITY: git-identical developer experience (user requirement, 2026-10-22)
Developers must be able to switch to asc without noticing the change. Where Scion and git UX conflict, git UX wins at the surface, and Scion's model stays underneath.
- **Same commands, flags, arguments, revision syntax, output text and exit codes** as git porcelain. That covers `HEAD~2`, `master..feat`, `@{u}`, pathspecs and `-p/--stat/--oneline`. `--porcelain`, `-z` and `--format=` must be byte-compatible for tools.
- **asc can be installed as `git`.** Use argv[0] detection or a shim, with fallthrough to real git for repos that are not asc. Scripts, editors and agents then keep working.
- **No IDE support (user decision, 2026-10-22).** ASC builds its own IDE plugins against asc's API: the RPC, local IPC and `--json` output. Do NOT build:
  - a full `.git` projection (git objects, refs mirror or rendered commits);
  - git's index as the staging area;
  - import of commits made by `git commit`;
  - a `git-remote-asc` promisor helper;
  - byte-compatibility work for IDE plumbing.

  Keep:
  - (a) A git-identical **CLI** for humans, scripts and agents, including `asc` installed as `git`. Claude Code and repo scripts such as `git ls-files` must work through it.
  - (b) A **minimal synthetic `.git` for Turborepo only**: `HEAD` plus a `.git/index` generated from tree metadata (git blob SHA-1 + size, already stored per entry), with asc-as-git answering turbo's few `rev-parse`/`ls-tree`/`status` calls. Turbo then hashes with zero hydration. This is a write-only export and not asc's staging area. asc keeps its own staging state.
  - (c) A stable, documented plugin API: the RPC plus a local daemon IPC with `Status`, `Diff`, `Log`, `Blame`, `Affected` and `Watch` events.
- **Same files and config.** Honour `.gitignore`, `.gitattributes`, `~/.gitconfig` and `.git/config`-style config, aliases, hooks (pre-commit, commit-msg and others), credential helpers, and `GIT_*` env vars where meaningful.
- **Same mental model.** Commit locally and then push. Branches behave like branches, and so do upstream tracking, `stash`, `rebase -i`, `cherry-pick`, `reset`, `restore` and `worktree add`.
  - Local commits are draft changesets. They are cheap and private until pushed, and the server may hold them.
  - Push is the ref compare-and-swap.
  - Branches are user- or workspace-scoped refs, so Scion's ban on global refs still holds.
- **Familiar errors and hints.** Use git's error wording ("Your branch is ahead of 'origin/master' by 1 commit.").
- **Unavoidable differences must be invisible or additive.** Examples: no `.git/objects`, and lazy files. New features go under new subcommands or flags and never change the behaviour of existing ones.
- **Compatibility test suite.** Run the same scenario scripts against git and asc and diff the output. This is an acceptance gate.

Every item below comes from the full Scion article (`scion-article.md`). SPEC.md must say how asc handles each one, or explicitly drop it with a reason.

## TOP PRIORITY: built-in polyglot project graph and `affected` (user requirement, 2026-10-22)
asc understands Rust, Python, C# and Bun/Node workspaces itself, and computes affected projects natively, as nx and turbo do. It does not depend on those tools.
- **Project detectors**, each a small pluggable module:
  - Cargo: workspace members, path deps and `[workspace.dependencies]`.
  - Python: `pyproject.toml` (uv workspaces, PEP 621 deps, path and editable sources) and `requirements*.txt`.
  - C#: `.sln`/`.slnx`, `.csproj`/`.fsproj` `ProjectReference`, `Directory.Build.props`/`Directory.Packages.props`.
  - Bun/Node: `package.json` workspaces and `workspace:` deps.
  - Optional overrides via an `asc.toml`/`.asc/projects.toml` (extra inputs, implicit deps, ignore globs, named inputs such as nx `namedInputs`).
- **Project graph as derived data per changeset.** It is computed recursively and incrementally. Only manifests touched by a changeset are re-parsed. It is served by the server API and needs no checkout.
- **Affected projects come from a server diff.** Run `asc affected [--base master] [--head HEAD] [--json] [--type rust|python|csharp|node] [--target build|test]`. Changed paths map to their owning projects, and the result is the transitive closure of reverse dependencies. Global inputs (root lockfiles, toolchain files, CI config, `.env*`) invalidate per the config. Lockfile-aware: a `bun.lock`/`Cargo.lock`/`uv.lock`/`packages.lock.json` diff marks only the projects whose resolved dependencies changed.
- **Task and input hashing** per project from tree hashes (a Merkle subtree hash of project inputs plus dependency project hashes), with no file hydration. Usable as a cache key, and able to feed Turbo/nx (`--filter`/`--projects`) or run tasks directly later.
- **CI output formats:** a list, JSON, a Turbo filter string, an nx projects list and a GitHub/Forgejo Actions matrix.
- **API-first.** CI can ask the server for "affected between merge-base and head" without any checkout.

## Principles
1. **Clients are always online.** Clients fetch on demand. asc also needs a local-only mode, which is a user requirement.
2. **Client-server asymmetry.** The server answers history, blame, commit, merge and rebase. The client keeps a bounded object cache, lays out the working copy, watches for file changes and gives a good UX. The client computes very little.
3. **API-first.** Every query the client uses is also a public API, so CI and tools such as automated bisect and revert need no local repo.
4. **Horizontal scalability.** The repo is never fully materialised anywhere, including the server. There are no packfiles and no repository maintenance (no gc or repack).
5. **Cheap, incremental, lock-free commits.** Commit, rebase and merge run server-side, so there is nothing to push. Cost scales with the size of the change, not the repo.
6. **Refs are discouraged** except master and release branches, because global agreed state does not scale. A ref move is a compare-and-swap on a single row and is separate from changeset creation. asc must reconcile this with git-like `branch` UX: workspace-local or user-scoped branches are cheap and not global.
7. **The working copy is optional and sparse by design.** Use sparse checkouts (directory sets) and virtual checkouts (materialise on read). Writes can skip checkout and go through the API.

## Data model
8. **A changeset is a WAL entry.** It records the files changed relative to the first parent, with content-addressed pointers to compressed contents. It is created server-side and visible everywhere instantly. First-parent storage is a stylistic choice that fits trunk-based work.
9. **Accepting changesets needs no packfiles, locking, cross-server coordination or ref moves.**
10. **One generic derived-data mechanism** defined recursively from parents. For each type, choose whether to compute it eagerly or defer it. It is invisible to clients, so it can be re-optimised. Types include blame, path history, commit-graph skip lists, copy detection and path search. Results are written back to the store.

## Fetching and tiers
11. **Content-addressed and immutable**, so any server can cache any object without coordination. Servers are "large caches that compute derived data on the fly".
12. **Negotiation is want-only.** The client never says what it has. It walks from the root tree, requesting missing trees recursively, then blobs. Each request is for a specific immutable object, so any cache can answer it. Transfers are resumable, with no multi-GB response.
13. **Sparse and virtual checkouts need no special protocol support.** The client simply does not ask.
14. **Per-node daemon over a socket** stores objects for every job on the node.
15. **The daemon boots from a periodically refreshed snapshot (seed)**, then talks only to its regional cache.
16. **Caches stream objects from memory in the daemon's on-disk format.** They do no per-client computation and no delta compression.
17. **Every tier speaks the same RPCs**, so clients do not know the topology.
18. **Tiering is strictly enforced** to isolate failure domains. A regional cache failure must not overwhelm upstream.
19. **Tiers hold no long-lived state.** Each keeps a cache hit rate above 99%.
20. **No repository maintenance** for clients or caches.

## Migration lessons
21. **Metadata first.** Before Scion had its own client, format or protocol, it served only metadata queries (ancestry, merge-base, blame, diff) for CI and tools. asc should do the same: ingest git and serve the metadata API before the VFS client exists.
22. **The git-to-store ingestion pipeline must preserve ordering invariants.** Optimise it for memory and speed.
23. **Do not brute-force lookups** such as finding when a change merged by reading the commit log linearly. Ask the review system or an index. Derived data plus a review/merge index covers this.

## Quality and operations
24. **Hermetic simulations** of every non-trivial infra and source-control interaction, plus fuzzing and rate limiting.
25. **Avoid over-engineering in response to hypothetical concerns.** Do not add excessive validation of trusted internal services.
26. **Repo-level ACLs plus directory-level permissions** for sensitive paths.
27. **Code review can express semantic approval rules**, for example comment-only changes versus logic changes. In asc this maps to Forgejo branch protection or the review service, with an asc semantic-diff classification API.
28. **Post-merge and pre-deploy testing instead of a global merge-queue lock.** Concurrent changes can conflict and agents fix them. The server-side rebase and merge API must make automated conflict resolution easy.
29. **Per-component monitoring ("superfans")**: telemetry and logs good enough for an agent to watch each component (daemon, cache, server). Structured metrics and logs per tier.
30. **Ancestry queries must use indexes** (skip lists or generation numbers). Never walk the history linearly.

## Scion's own advice for git-based shops
31. **Server-side RPCs**, ideally standardised across forges.
32. **Negotiation can change.** Sparse and shallow features should compose. Relevant to asc's legacy git bridge for Forgejo.

## TOP PRIORITY: Scion-core memory and storage efficiency (user requirement, 2026-10-22)
Scion's efficiency model is the core of asc, not an optional later step. The following items are v1 requirements. They are no longer "not built in v1".
- **Want-only tree-then-blob fetch.** Fetches are batched, streamed and resumable. Objects travel in the exact stored or on-disk format, with no re-encoding and no per-client delta computation. Every tier answers from its cache.
- **Tiers that speak the same RPCs:** daemon, then `asc serve --role cache` (a stateless read-through cache, same binary), then the authoritative server. Tiering is strict, with per-tier rate limits and shedding so a cache failure cannot stampede upstream.
- **Seed snapshots.** The server periodically publishes a snapshot of the master tip (trees plus hot manifest blobs) to the bucket. Daemons and CI boot from it, then fetch only the delta.
- **An ancestry index as derived data:** generation numbers plus skip lists. Merge-base, `is-ancestor` and log ranges never walk history linearly.
- **Explicit memory budgets**, enforced by tests and benchmarks:
  - The daemon's idle RSS is at most 64 MiB and is bounded under load.
  - The server streams responses and never buffers a whole file or tree set. Its caches are size-capped.
  - The CLI never loads the full tree. It walks lazily, by spine.
- **Explicit storage budgets per machine:**
  - Each object is stored once per machine, shared by all workspaces, worktrees and repos.
  - The cache is bounded with LRU eviction, and hydrated files can be released again (dehydration).
  - There are no packfiles, no gc and no cruft.
  - Report bytes per workspace, which is about 0 before hydration.
- **Storage efficiency in the bucket:**
  - zstd compression, with trained dictionaries for small text and JSON objects as a measured v1.x item.
  - Small objects packed into immutable segments, to cut S3 request and per-object overhead, as a measured v1.x item.
  - Dedup across a fork network where visibility allows.
- **Derived data is lazy by default.** It is computed when first asked for and written back. Only master-critical types are eager (gen, skip lists, gitcommit, project graph).
- **A benchmark suite in CI** tracks bytes fetched, bytes on disk, peak RSS and latency for clone, switch, status, diff, affected and CI checkout on the ASC workload. A regression fails the build.

## Ignore files: `.ascignore` first, `.gitignore` as fallback (user decision, 2026-10-22)
- **Per-directory precedence.** If a directory contains a `.ascignore`, asc uses it and does not read that directory's `.gitignore`. Otherwise asc reads `.gitignore`. Files are never merged, so the rule stays predictable.
- **Syntax and semantics are identical to gitignore:** negation, `**`, anchoring, trailing `/`, escaping, and nested files scoped to their own directory. One parser serves both names.
- **Repo-local excludes:** `.asc/info/exclude`, with `.git/info/exclude` as fallback.
- **Global excludes:** `asc config core.excludesFile`, defaulting to `~/.config/asc/ignore`, with git's `core.excludesFile` as fallback.
- **Commands:** `asc check-ignore -v` reports which file and line matched. `asc init` creates a `.ascignore`. `asc migrate ignore` renames or copies `.gitignore` to `.ascignore`.
- **Git mirror export** carries both files as ordinary tracked content.
- **Same pattern for attributes:** `.ascattributes` first, with `.gitattributes` as fallback.

## Own Actions fork: CI uses asc, not git (user decision, 2026-10-22)
- **Fork the core actions to speak asc:** `checkout`, `cache` and `upload/download-artifact`, mirrored under `asc/actions` on Forgejo, plus new `asc/setup`, `asc/affected` and `asc/turbo-cache`.
  - `checkout` calls `asc ci checkout` with virtual, sparse or plain modes and falls back to git only for repos that are not asc.
  - `cache` and artifacts store their data in the asc object store, so the CAS dedupes it.
- **Transparent adoption.** Point the Forgejo fork's `[actions] DEFAULT_ACTIONS_URL` at the mirror, and add a runner-fork alias table: `actions/checkout@*` maps to `asc/actions/checkout`. Existing workflows then switch with no YAML edits.
- **Runner-native fast path.** The runner fork recognises the checkout action and runs the asc checkout provider on the host, which shares the daemon cache, instead of running a node action in the container.
- **Same inputs and outputs as upstream** (`ref`, `fetch-depth`, `path`, `submodules`, `lfs`, `sparse-checkout`), mapped to asc semantics. Upstream workflows keep working.

## Benchmark against git: asc must be faster (user requirement, 2026-10-22)
- **A head-to-head benchmark harness** (`bench/`, hyperfine-driven plus a Rust criterion suite for internals). It runs the same scenarios on git and asc against the monorepo workload, imported, and a synthetic scaled repo at 10x and 100x files and history. Scenarios:
  - clone (full, blobless and shallow for git, against asc virtual and plain);
  - `worktree add`;
  - `status` clean and dirty;
  - `switch` between tips;
  - `diff` (HEAD, merge-base, `--stat`);
  - `log` (path-limited);
  - `blame`;
  - `merge-base` / `is-ancestor`;
  - `commit` / `push` / `fetch`;
  - CI checkout plus affected;
  - turbo `--dry` hashing.
- **Metrics:** wall time p50/p95, bytes transferred, bytes on disk, peak RSS, files hydrated and server CPU.
- **Gate:** asc must beat git on every scenario where Scion's model applies: clone, worktree, switch, CI checkout, merge-base, blame, log, disk and transfer. It must be within 10% elsewhere, such as hot local `diff` or `status`. CI fails on regression against the stored baseline.
- **Results are published** to `docs/BENCHMARKS.md`: a table for each release with the git version, machine spec and reproduction commands.

## Checkout speed: small and HUGE monorepos (user requirement, 2026-10-22)
- **Benchmark tiers for checkout** (clone, `worktree add`, `switch`, CI checkout), against git's best options each time: full, `--filter=blob:none`, `--depth 1`, sparse-checkout, and scalar where it applies.
  - **Small:** monorepo, about 4.5k files.
  - **Medium:** about 80k files and 1M commits, imported from a public repo such as the linux kernel.
  - **HUGE:** chromium-scale, at least 400k files and a 25+ GB working copy. Use a synthetic generator (`bench/gen`): a deterministic repo with configurable files, depth, size distribution, history length and churn, including 1M-file and 100 GB variants. Optionally also import a chromium snapshot.
- **Measured:** time to first usable shell, time to first build, bytes transferred, bytes on disk, files hydrated and peak RSS, both cold (empty machine cache) and warm (seed snapshot or shared cache).
- **Goal, following Scion:** HUGE-repo virtual checkout in seconds, independent of repo size. Cost must scale with the files read, not the files in the repo. The `worktree add` cost must not depend on repo size.

## No LFS (user decision, 2026-10-22)
asc has no LFS support.
- Large files are native chunked content in the CAS, with no setup needed.
- Git import keeps LFS pointer files as ordinary small blobs, byte-exact, so git SHAs and the mirror stay identical. LFS content is never fetched or migrated. The 25.6 GiB LFS store is archived once to a cold S3 bucket, outside asc.
- Removed: `Entry.lfs`/`Change.lfs`, the `x/sha256` alias, the SHA-256 computed during ingest, pointer parsing, Forgejo LFS API serving and the LFS copy job.
