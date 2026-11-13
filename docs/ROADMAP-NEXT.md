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
| 3 | Local durability audit | Every object write and rename is synced before a ref names it; fix what is not. Done: directory sync in the object store, demote and pack cache; fsync on a local-directory server store. Open: the SQLite synchronous level on the server and the client. | S | in progress |
| 4 | Backup chain check | Backup verifies that every delta chain restores | S | open |

## 2. Storage size

| # | Item | What it does | Size | Status |
|---|---|---|---|---|
| 5 | Cold-only deltas | Only history past a set age or distance from every tip may become a delta | D | decision |
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

## 4. Agents and merging

| # | Item | What it does | Size | Status |
|---|---|---|---|---|
| 16 | First-class conflicts | A conflicted change can be committed and resolved later, so restack and landing never block an unattended agent. Out of scope in SPEC section 0 today | D | decision |
| 17 | Structural merge driver | A tree-sitter merge for the five indexed languages | S | open |
| 18 | `WatchRefs` | A pushed notice when a ref moves, in place of polling by agents, CI and the IDE plugins. Out of scope today | S | open |

## 5. CI and build tools

| # | Item | What it does | Size | Status |
|---|---|---|---|---|
| 19 | sccache adapter | Rust and C++ compile cache on the same store | S | open |
| 20 | Bazel remote cache adapter | Also serves Buck2 and Pants | S | open |
| 21 | Gradle, Nx and BuildKit adapters | The same pattern for three more tools | S | open |
| 22 | `node_modules` snapshots | An install stored as a tree keyed by lockfile, mounted instead of downloaded. Out of scope today | D | decision |
| 23 | Virtual CI checkout on Windows and macOS | `asc ci checkout --mode virtual` is documented for Linux runners only | S | open |
| 24 | Task runner (`asc run`) | asc has the graph, the hashes and the cache. Out of scope today | D | decision |
| 25 | asc's CI builds its tests once | One job compiles and archives the test binaries; four shards run the archive (was three shards each compiling for 5 to 7 minutes). The rust jobs now finish in about 11 to 14 minutes depending on the artifact download; the longest job is `projfs` (12 minutes) | S | done |

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
| 26 | Read-only git view | Enough for `git status`, `git diff` and `git log` inside an asc workspace, so tools that run git keep working | D | decision |
| 27 | File locking for binary files | Only for teams with large binary assets | D | skip |
| 35 | Merge queue with speculative CI | Not built by decision: post-merge testing (SPEC section 12.6) | | skip |

## Order of work

1. Safety and the delta rule: 3, 6, 7, 28, 32, then 1 and 2.
2. Disk: 10, 14, 15.
3. Agents: 11, then 16, 17, 18.
4. CI reach: 19, 20, 23, 25.
5. Vulnerability awareness: 29, 31, 33, 36 to 40, then 30 and 34.
6. Later: 4, 8, 9, 12, 13, 21, 22, 26.

Rows 5, 16, 22, 24 and 26 change a decision the specification records; each waits for the owner.
