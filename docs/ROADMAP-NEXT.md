# Roadmap: next work

Written 2026-11-12 from a review of the worktree code, `docs/SPEC.md` (outline and out-of-scope
list), `docs/STATUS.md` and the dependency service. Each row says what is missing as far as that
review checked; a row marked **check first** needs a short read of the existing code before work.

Goals, in the owner's order: storage that is safe, small and fast to read; worktrees with no
duplicate files; fast CI; build tools served from the same store; and source control as a service
that does more than store and serve files.

**Status:** `open`, `in progress`, `done`, `decision` (needs the owner's decision first), `skip`.
**Size:** `S` is bounded work with a named acceptance test; `D` changes the data model, the storage
format or GC and needs a design before any code.

## Principle: full objects where speed matters

Reads, caches and clients only ever see full, self-contained objects. A delta is cold-storage
compaction on the server, never a wire or cache format. Checkout time and CPU cost more than
object storage, so a store somewhat larger than a git pack is accepted. Deltas inside packs were
considered and rejected for this reason.

## 1. Storage safety

| # | Item | What it does | Size | Status |
|---|---|---|---|---|
| 1 | Server scrub | A slow, resumable job re-reads stored objects, checks hashes (delta bases first), reports, and repairs from a second copy | D | open |
| 2 | Object-lock mode | Optional bucket versioning or object lock with a delete delay, so a GC fault or a leaked key cannot destroy history | D | open |
| 3 | Local durability audit | Every object write and rename is synced before a ref names it; fix what is not. Done: directory sync in the object store, demote and pack cache; fsync on a local-directory server store. The server and client SQLite databases (refs, changesets, queue) run WAL with synchronous FULL, set at open and tested; `store_local_fsync` (default true) opts a local-directory store out of the sync. | S | done |
| 4 | Backup chain check | Backup verifies that every delta chain restores | S | open |

## 2. Storage size

| # | Item | What it does | Size | Status |
|---|---|---|---|---|
| 5 | Cold-only deltas | Only history past a set age or distance from every tip may become a delta Decided 2026-11-13: yes, cold history only. | D | open |
| 6 | Never serve a delta | A written rule and a test: every read path, cache and client receives full values | S | done |
| 7 | Cold-read benchmark | Latency of reading a full-depth chain (seven links) from a store with request latency, recorded in `docs/BENCHMARKS.md`, before deltas are enabled anywhere | S | done |
| 8 | Fork deduplication | Forks share objects instead of copying them | D | open |
| 9 | Large-file chunk pairing | Chunks are 65 MB of the 192.6 MB dash store; git packs the same files in 50 MB | S | open |
| 10 | Reflink files over 4 MiB | One assembled copy per machine, cloned into each plain worktree | S | done |

## 3. Worktrees and the working copy

| # | Item | What it does | Size | Status |
|---|---|---|---|---|
| 11 | Continuous snapshots | The daemon snapshots a virtual worktree when it goes idle, so `asc undo` covers any change and a crashed agent's work is recoverable | D | open |
| 12 | Snapshot sync | Snapshots go to the commit cloud: work in progress follows the user between machines | D | after 11 |
| 13 | Agent overlay worktrees | Writes land in a small overlay; the rest is served from the store. Measure the disk use of an agent worktree first | D | open |
| 14 | `asc doctor` picks the mode | Detects reflink and virtual support, states the choice and what it costs | S | done |
| 15 | Link mode as last resort | Documented so; doctor recommends virtual mode or a volume that clones | S | done |
| 41 | One virtual file system core, measured | Research first: benchmark ProjFS, Cloud Files, FUSE, NFS and FSKit on the same workload (cold open, warm read, directory walk, status, write, switch), then decide whether one Rust core behind a single transport can beat them on every system. Candidates: a loopback SMB3 server (Windows 11 24H2 and later accept another port), loopback NFSv4 everywhere, or keeping the native drivers behind one shared core. A kernel driver of our own is out: it needs driver signing on Windows and macOS | D | open |

## 4. Agents and merging

| # | Item | What it does | Size | Status |
|---|---|---|---|---|
| 16 | First-class conflicts | A conflicted change can be committed and resolved later, so restack and landing never block an unattended agent. Out of scope in SPEC section 0 today Decided 2026-11-13: yes; a design comes first. | D | open |
| 17 | Structural merge driver | A tree-sitter merge for the five indexed languages | S | open |
| 18 | `WatchRefs` | A pushed notice when a ref moves, in place of polling by agents, CI and the IDE plugins. Out of scope today | S | open |

## 5. CI and build tools

| # | Item | What it does | Size | Status |
|---|---|---|---|---|
| 19 | sccache adapter | Rust and C++ compile cache on the same store | S | open |
| 20 | Bazel remote cache adapter | Also serves Buck2 and Pants | S | open |
| 21 | Gradle, Nx and BuildKit adapters | The same pattern for three more tools | S | open |
| 22 | `node_modules` snapshots | An install stored as a tree keyed by lockfile, mounted instead of downloaded. Out of scope today Decided 2026-11-13: yes; a design comes first. | D | open |
| 23 | Virtual CI checkout on Windows and macOS | `asc ci checkout --mode virtual` is documented for Linux runners only | S | open |
| 24 | Task runner (`asc run`) | asc has the graph, the hashes and the cache. Out of scope today Decided 2026-11-13: yes, as an optional module that is installed and set up separately; a design comes first. | D | open |
| 25 | asc's CI builds its tests once | One job compiles and archives the test binaries; four shards run the archive (was three shards each compiling for 5 to 7 minutes). The rust jobs now finish in about 11 to 14 minutes depending on the artifact download; the longest job is `projfs` (12 minutes) Decided 2026-11-13: keep this layout and cut the `projfs` job next. | S | done |
## 6. Vulnerability awareness

The dependency service (`crates/asc-serve/src/deps/`, `docs/DEPENDENCIES.md`) already reads
manifests and lockfiles without running repository code, matches packages against OSV (or a mirror
or an advisory file), scores by CVSS, can refuse a landing by severity and proposes updates as
draft changes. These rows extend it. The model to match is a scanner that runs on the operator's
own server and sends no repository data to a third party.

| # | Item | What it does | Size | Status |
|---|---|---|---|---|
| 28 | Exploited-in-the-wild ranking | CISA's Known Exploited Vulnerabilities list and EPSS scores beside CVSS, from mirrored files | S | open |
| 29 | Re-scan trunk on new advisories | What has already landed is checked again when advisories change, with a notice. Checked: scans run only on request and at landing; nothing reacts to a new advisory | S | open |
| 30 | Reachability | A finding is marked "used" only when the code index shows the package or symbol is referenced. Checked: `deps/imports.rs` collects the npm packages the sources import, but nothing reads the set | D | open |
| 31 | Images and CI actions | Dockerfile base images and pinned CI actions are scanned as well as language packages Checked: none of the three is parsed | S | open |
| 32 | SBOM per revision | CycloneDX or SPDX for any changeset, from the data already parsed | S | open |
| 33 | Advisories against asc itself | The server and the CLI tell the operator when the running version has an advisory. Checked: not present | S | open |
| 34 | Cache and artifact marking | Build-cache entries and CI artifacts built with a dependency that later got an advisory are flagged | D | open |
| 36 | One security status | Each repository and each change has one status (clean, findings, unknown) with the versions to move to | S | open |
| 37 | Security page across repositories | One page per owner or organisation listing every repository's status and findings, with export | S | open |
| 38 | Notifications | A notice to the repository's watchers when its status changes | S | open |
| 39 | Agent tools | MCP tools that read findings and manage scanning | S | open |
| 40 | Scanner limits | A total time limit and a CPU limit per scan. Checked: file, byte, package and concurrency limits exist; these two do not | S | open |

## 7. Adoption

| # | Item | What it does | Size | Status |
|---|---|---|---|---|
| 26 | Read-only git view | Enough for `git status`, `git diff` and `git log` inside an asc workspace, so tools that run git keep working Decided 2026-11-13: yes; a design comes first. | D | open |
| 27 | File locking for binary files | Only for teams with large binary assets | D | skip |
| 35 | Merge queue with speculative CI | Not built by decision: post-merge testing (SPEC section 12.6) | | skip |

## 8. The hub

From a review of the hub's asc pages on 2026-11-13 (routes, templates and styles; nothing was run). Agents, review, stacks, the inbox, settings and admin pages exist. These rows are what the owner chose to add.

| # | Item | What it does | Size | Status |
|---|---|---|---|---|
| 42 | Cache and storage insights | CI time saved by the cache, cache hit rate, and storage used and saved, on the Insights page | S | open |
| 43 | Agent proposals inbox and live map | One queue of agent proposals to approve or reject, in the inbox, and a live view of which agent works on what | S | open |
| 44 | Worktrees view | A page listing the live virtual worktrees of a repository and who holds them | D | open |
| 45 | Commit-cloud view | All branchless work by user and agent, with restore | D | open |
| 46 | Affected projects and cache hits on a change | Shown inline with the checks of a change | S | open |
| 47 | Land a stack in one click | The stack view shows each change's status and has a "land stack" action | S | open |
| 48 | Action palette and key help | The palette runs actions (restack, land, rerun, open agent) as well as search; `?` shows the keys | S | open |
| 49 | API and webhook reference page | Generated from the RPC schema | S | open |
| 50 | Repository templates | Templates in the create flow | D | open |
| 51 | Own pages for repo home, diff and issues | Rebuilt in the asc design system, with the loose `asc-*.css` files folded into it, so no page is the upstream forge's template with a new skin | D | open |
| 52 | SBOM download | On the dependency page (the server side is row 32); the owner-wide security page is row 37 | S | after 32 |
| 53 | `projfs` CI job under 8 minutes | It is the longest job (12 minutes: 5 building, 4 testing) | S | open |

## Order of work

1. Safety and the delta rule: 3, 6, 7, 28, 32, then 1 and 2.
2. Disk: 10, 14, 15.
3. Agents: 11, then 16, 17, 18.
4. CI reach: 19, 20, 23, 25.
5. Vulnerability awareness: 29, 31, 33, 36 to 40, then 30 and 34.
6. Later: 4, 8, 9, 12, 13, 21, 22, 26.

Rows 5, 16, 22, 24 and 26 change a decision the specification records. The owner decided all five on 2026-11-13 (see each row); 16, 22, 24 and 26 start with a design.

7. The hub: 46, 47, 48, 42, 43, 49, 52, then 44, 45, 50, 51.
8. Virtual file systems: 53, then 41.
