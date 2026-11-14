# Roadmap

This is the one tracking document. It replaces the status page, the "next work" roadmap, the plan audit, the two gap lists and the naming note. The specification is `docs/SPEC.md`; the server contract is `docs/BUILDOUT.md`. The original implementation plan is kept as history in `docs/history/PLAN.md`. Items that the specification or other documents put out of scope are reviewed in `docs/OUT-OF-SCOPE.md`.

**Status:** `open`, `in progress`, `done`, `decision` (needs the owner's decision first), `skip`. Some rows wait on another row (`after 11`) or on a measurement.
**Size:** `S` is bounded work with a named acceptance test. `D` changes the data model, the storage format or GC and needs a design before any code.
**Check first:** the row needs a short read of the existing code before work.

Goals, in the owner's order: storage that is safe, small and fast to read; worktrees with no duplicate files; fast CI; build tools served from the same store; and source control as a service that does more than store and serve files.

## Principle: full objects where speed matters

Reads, caches and clients only ever see full, self-contained objects. A delta is cold-storage compaction on the server, never a wire or cache format. Checkout time and CPU cost more than object storage, so a store somewhat larger than a git pack is accepted. Deltas inside packs were considered and rejected for this reason.

## 1. Where things stand

Audited 2026-10-30. Updated 2026-11-13 at head dcd3fbf9. Every row of that audit was checked by reading code or tests, by grep, or by the CI runs; "the docs say so" was not taken as evidence.

What is verified:

- Release `v0.1.4` was published 2026-11-11 with every file signed and `SHA256SUMS` checked in the run. Tags `v0.1.0` to `v0.1.4` exist.
- `scripts/install.sh` installed the published `v0.1.4` Linux archive with the signature checked, and refused it under another key (2026-11-11). The `0.1.3` self-update rehearsal updated itself to `0.1.4`.
- CI jobs on master: `rust`, `build-cache`, `ts`, `web-e2e`, `postgres`, `garage` (v1.3.1, v2.3.0, v2.4.1), `windows`, `projfs`, `jetbrains`, `macos` and `fskit-swift`.
- `asc doctor --store` ran against a Garage v2.5.0 bucket on `<staging host>` over https (recorded in `docs/BYOS.md`).
- Packed objects, zstd dictionaries, delta storage, import batching, the path log, the rename memo, go to file and the daemon-side turbo tier are on master.
- Every plan unit is implemented except hub unit K15 (row 75). K16 is reviewed in `docs/OUT-OF-SCOPE.md`.

What is not verified:

- The counts (33 services, 272 RPCs, 216 called by the hub, the test counts) are from the 2026-10-30 audit and were not recounted.
- One test was run for the audit: `cargo test -p asc-core --lib transport_policy` (8 passed). The full workspace was not rebuilt; CI is the gate for that.
- The acceptance commands of the plan audit were not run again.
- The TSRX workflow template commands were not run against real npm packages (row 74).
- JetBrains `verifyPlugin` runs in CI only. The MCP Inspector runs only on the Forgejo runner (`mcp-inspector.yml`).
- The `macos` job runs the `asc-vfs` tests and the `asc` unit tests only, not the whole workspace (row 72). Fuzz and bench run only in `.forgejo/workflows`.
- The MSI is built only by the Forgejo flow (row 70).
- `install.sh` needs `minisign` installed to check a signature, and the GitHub release is not in the installer layout (row 71).
- SSH is not verified on a deployed server.
- A real FSKit mount needs a Developer ID signature; CI builds ad hoc only (row 64).
- No run against AWS S3 or a real AI provider. `TestKey` waits for the owner's key (row 73).
- The hub's insights rows read `needs-both` as of 2026-10-28 (row 66). 56 RPCs are not called by the hub (row 65).
- The queue ETA shows the position only (row 67). Agent budgets have no monthly window (row 68).
- The IDE plugins are not on the marketplaces (row 69).
- Packs and deltas are off by default (row 63). On the benchmark history the store is 192.6 MB against git's 161 MB pack (`docs/BENCHMARKS.md`).
- The standalone token store is a second token system (row 61). There is no native operating-system credential store (row 62).

In flight: rows 42 and 46 (the cache statistics store and recorder have landed; the hub Insights page and the inline view on a change remain), rows 4, 5, 36 and 79 (first units in progress).

## 2. Rows

### Done

| # | Item | Result |
|---|---|---|
| 3 | Local durability audit | Directory sync in the object store, demote and pack cache; fsync on a local-directory store; SQLite in WAL with synchronous FULL; `store_local_fsync` opts out |
| 6 | Never serve a delta | A written rule and a test: every read path, cache and client receives full values |
| 7 | Cold-read benchmark | A seven-link chain read with request latency, recorded in `docs/BENCHMARKS.md` |
| 10 | Reflink files over 4 MiB | One assembled copy per machine, cloned into each plain worktree |
| 14 | `asc doctor` picks the mode | Detects reflink and virtual support and states the choice and its cost |
| 15 | Link mode as last resort | Documented; doctor recommends virtual mode or a volume that clones |
| 25 | asc's CI builds its tests once | One job builds and archives the test binaries; four shards run the archive |
| 47 | Land a stack in one click | Lands bottom-first through the single-change path, behind a dry-run page |
| 48 | Action palette and key help | The palette runs actions as well as search; `?` shows the keys |
| 49 | API and webhook reference page | Hub page `/-/api`, generated from the RPC schema (hub commits 6700eac470 and daacdc76ae) |
| 53 | `projfs` CI job under 8 minutes | The test step no longer calls cargo twice; 7 minutes 23 seconds |

### Storage safety

| # | Item | What it does | Size | Status |
|---|---|---|---|---|
| 1 | Server scrub | A slow, resumable job re-reads stored objects, checks hashes (delta bases first), reports, and repairs from a second copy | D | open |
| 2 | Object-lock mode | Optional bucket versioning or object lock with a delete delay, so a GC fault or a leaked key cannot destroy history | D | open |
| 4 | Backup chain check | Backup verifies that every delta chain restores | S | open |

### Storage size

| # | Item | What it does | Size | Status |
|---|---|---|---|---|
| 5 | Cold-only deltas | Only history past a set age or distance from every tip may become a delta. Decided 2026-11-13: yes, cold history only | D | open |
| 8 | Fork deduplication | Forks share objects instead of copying them. Shared `BucketRefs` exist for one bucket (`--refs shared`); the row covers sharing objects and refs across a fork network (SPEC section 3.3) | D | open |
| 9 | Large-file chunk pairing | Chunks are 65 MB of the 192.6 MB benchmark store; git packs the same files in 50 MB | S | open |

### Worktrees and the working copy

| # | Item | What it does | Size | Status |
|---|---|---|---|---|
| 11 | Continuous snapshots | The daemon snapshots a virtual worktree when it goes idle, so `asc undo` covers any change and a crashed agent's work is recoverable | D | open |
| 12 | Snapshot sync | Snapshots go to the commit cloud: work in progress follows the user between machines. This is also the SPEC's "workspace sync" item | D | after 11 |
| 13 | Agent overlay worktrees | Writes land in a small overlay; the rest is served from the store. Measure the disk use of an agent worktree first | D | open |
| 41 | One virtual file system core, measured | Benchmark ProjFS, Cloud Files, FUSE, NFS and FSKit on one workload, then decide whether one Rust core behind one transport beats them all. Candidates: loopback SMB3, loopback NFSv4, or the native drivers behind one shared core. A kernel driver of our own is out: it needs driver signing on Windows and macOS | D | open |

### Agents and merging

| # | Item | What it does | Size | Status |
|---|---|---|---|---|
| 16 | First-class conflicts | A conflicted change can be committed and resolved later, so restack and landing never block an unattended agent. Decided 2026-11-13: yes. Design: `docs/design/FIRST-CLASS-CONFLICTS.md` (17 units) | D | in progress |
| 17 | Structural merge driver | A tree-sitter merge for the five indexed languages | S | open |
| 18 | `WatchRefs` | A pushed notice when a ref moves, in place of polling. **Check first**: the RPC is built (`crates/asc-serve/src/watch_refs.rs`) and the daemon uses it; the row covers the callers that still poll (CI, the IDE plugins, the hub) | S | open |
| 76 | Pause at the AI cap | `stop_at_cap` pauses an agent run at its cap so it can resume; today the run ends with `AI_CAP_REACHED` (`agents/runtime.rs`) | S | open |
| 79 | Person profile tool for agents | The sandbox tool list and the MCP toolset get a tool that reads a person's profile (`Agents.GetPerson` exists) | S | open |

### CI and build tools

| # | Item | What it does | Size | Status |
|---|---|---|---|---|
| 19 | sccache adapter | Rust and C++ compile cache on the same store | S | open |
| 20 | Bazel remote cache adapter | Also serves Buck2 and Pants | S | open |
| 21 | Gradle, Nx and BuildKit adapters | The same pattern for three more tools | S | open |
| 22 | `node_modules` snapshots | An install stored as a tree keyed by lockfile, mounted instead of downloaded. Decided 2026-11-13: yes. Design: `docs/design/DEPENDENCY-SNAPSHOTS.md` (13 units) | D | in progress |
| 23 | Virtual CI checkout on Windows and macOS | `asc ci checkout --mode virtual` is documented for Linux runners only | S | open |
| 24 | Task runner (`asc run`) | asc has the graph, the hashes and the cache. Decided 2026-11-13: yes, as an optional module installed and set up separately. Design: `docs/design/TASK-RUNNER.md` (17 units) | D | in progress |
| 85 | More stores in the storage CI | MinIO joins the Garage matrix in CI, and one run against Cloudflare R2 is recorded in `docs/BYOS.md`. R2 needs the owner's account | S | open |

### Vulnerability awareness

The dependency service (`crates/asc-serve/src/deps/`, `docs/DEPENDENCIES.md`) reads manifests and lockfiles without running repository code, matches packages against OSV, scores by CVSS, can refuse a landing by severity and proposes updates as draft changes. These rows extend it. The model is a scanner on the operator's own server that sends no repository data to a third party.

| # | Item | What it does | Size | Status |
|---|---|---|---|---|
| 28 | Exploited-in-the-wild ranking | CISA's Known Exploited Vulnerabilities list and EPSS scores beside CVSS, from mirrored files. The feeds, the exploited-first sort and the KEV and EPSS gate have landed | S | done |
| 29 | Re-scan trunk on new advisories | What has landed is checked again when advisories change, with a notice. Today scans run only on request and at landing | S | open |
| 30 | Reachability | A finding is marked "used" only when the code index shows the package or symbol is referenced. `deps/imports.rs` collects imports, but nothing reads the set | D | open |
| 31 | Images and CI actions | Dockerfile base images and pinned CI actions are scanned as well as language packages | S | open |
| 32 | SBOM per revision | CycloneDX or SPDX for any changeset. The SBOM RPC and `asc deps sbom` have landed | S | done |
| 33 | Advisories against asc itself | The server and the CLI tell the operator when the running version has an advisory | S | open |
| 34 | Cache and artifact marking | Build-cache entries and CI artifacts built with a dependency that later got an advisory are flagged | D | open |
| 36 | One security status | Each repository and each change has one status (clean, findings, unknown) with the versions to move to | S | open |
| 37 | Security page across repositories | One page per owner or organisation listing every repository's status and findings, with export | S | open |
| 38 | Notifications | A notice to the repository's watchers when its status changes | S | open |
| 39 | Agent tools | MCP tools that read findings and manage scanning | S | open |
| 40 | Scanner limits | A total time limit and a CPU limit per scan. File, byte, package and concurrency limits exist | S | open |
| 86 | Scan progress shared between processes | Scan progress is kept in the database, so a restart or a load balancer does not lose it. Today it is held in the memory of the scanning process (`docs/DEPENDENCIES.md`, "Progress") | S | open |

### Adoption

| # | Item | What it does | Size | Status |
|---|---|---|---|---|
| 26 | Read-only git view | Enough for `git status`, `git diff` and `git log` inside an asc workspace, and for the IDEs' built-in git. Decided 2026-11-13: yes. Design: `docs/design/GIT-VIEW.md` (12 units) | D | in progress |
| 27 | File locking for binary files | Only for teams with large binary assets | D | skip |
| 35 | Merge queue with speculative CI | Not built by decision: post-merge testing (SPEC section 12.6) | | skip |

### The hub

From a review of the hub's asc pages on 2026-11-13 (routes, templates and styles; nothing was run).

| # | Item | What it does | Size | Status |
|---|---|---|---|---|
| 42 | Cache and storage insights | CI time saved by the cache, cache hit rate, and storage used and saved, on the Insights page | S | open |
| 43 | Agent proposals inbox and live map | One queue of agent proposals to approve or reject, and a live view of which agent works on what | S | done |
| 44 | Worktrees view | A page listing the live virtual worktrees of a repository and who holds them | D | open |
| 45 | Commit-cloud view | All branchless work by user and agent, with restore | D | done |
| 46 | Affected projects and cache hits on a change | Shown inline with the checks of a change | S | open |
| 50 | Repository templates | Templates in the create flow | D | done |
| 51 | Own pages for repo home, diff and issues | Rebuilt in the asc design system, with the loose `asc-*.css` files folded in, so no page is the upstream forge's template with a new skin | D | open |
| 52 | SBOM download | On the dependency page (the server side is row 32) | S | done |
| 75 | Hub integration matrix over both storage backends | The hub's integration tests run with git storage and with asc storage (plan unit K15, `FORGEJO_TEST_STORAGE_BACKEND`). **Check first** | S | open |
| 80 | Pull mirrors stored in asc | Pull mirrors and wiki pull mirrors write into an asc repository; today the mirror service has no asc path | S | open |
| 81 | Signed commits from web edits | A web edit on a branch that requires signatures cannot be signed today. Decided 2026-11-14: (a) the server signs web edits with an instance key. | S | open |
| 84 | Issue counts per state and due dates | The hub's issue pages show counts per state and due dates (hub `UI-COVERAGE.md`) | S | open |
| 87 | Mirrors stored in asc | A pull mirror can be asc-stored: each sync imports the outside repository's history into asc the way a one-time migration does (hub `services/repository/migrate_asc.go`, `ascsync.ImportHistory`). Today mirrors are always git-stored (`newRepoStorageBackend` in hub `services/repository/create.go`) | D | open |
| 88 | Move a git-stored repository to asc | An owner action converts an existing git-stored repository to asc storage in place, keeping its name, issues, pull requests and settings; history is imported in the background with the same steps as a migration. Today the only path is an import into a new repository | D | open |

### Server and accounts

| # | Item | What it does | Size | Status |
|---|---|---|---|---|
| 77 | Administrator read of one person's usage | A site administrator can read one person's usage. **Check first**: read `usage/` | S | done |
| 78 | Live restore | A restore runs while the server stays up: read-only mode while it runs, and no manual restart after | D | open |
| 82 | CLI for the account services | A standalone server without the hub can drive the account services (sessions, export, usage, org, backups) from `asc serve` subcommands; today the hub is the only client | S | open |
| 83 | OpenSSH forced-command entry | An entry point for a system OpenSSH `command=` line, so an operator can serve asc behind sshd (SPEC section 25.11.1) | S | open |

### Brought into scope and carried over (2026-11-13)

| # | Item | What it does | Size | Status |
|---|---|---|---|---|
| 54 | asc RPC over SSH | The asc RPC protocol runs over the SSH listener, with `asc-authenticate` (SPEC section 25.11.9). Git over SSH already works | D | open |
| 55 | SSH certificates and security keys | The `asc serve` SSH listener accepts user certificates and `sk-*` keys (SPEC section 25.11.1); today `authz/ssh_keys.rs` refuses them | S | open |
| 56 | TypeScript import-graph inference | The project graph gets edges from TypeScript imports, not only from manifests. It feeds row 30 and affected projects | D | open |
| 57 | Lockfile parsing for the project graph | The project graph reads the npm, pnpm, yarn and bun lockfiles. **Check first**: the dependency service already parses lockfiles (`crates/asc-serve/src/deps/lock/`), so the row is to reuse them | S | open |
| 58 | Host-level fetch proxy for CI | One proxy per CI host serves object fetches to all jobs on it. Build only if a measurement shows a gain | S | open, gated on measurement |
| 59 | Remaining git commands | `shortlog`, `name-rev`, `whatchanged`, `range-diff`, `cherry` and `notes` run natively. **Check first**: these are built (`crates/asc/src/cli/{shortlog,name_rev,whatchanged,range_diff,cherry,notes}.rs`); the row is to confirm against the compat suite and close | S | open |
| 60 | Signing settings page | A settings page in the IDE plugins for the signing keys that `asc config` holds. Today the plugins show a "signed" badge only. Decided 2026-11-14: (a) build the page. | S | open |
| 61 | One token system | The standalone token store (`authz/token_store.rs`) and the hub's tokens become one system. **Check first**: `docs/DECISIONS/0002-one-token-system-per-server.md` already allows one system per server | S | open |
| 62 | Native credential store | The CLI stores credentials in the operating system's store. **Check first**: `crates/asc/src/credential.rs` has an asc keystore for when no helper is set | S | open |
| 63 | Packed objects on by default | `[pack] enabled` defaults to true, after a measurement on a real history | S | open |
| 64 | Real FSKit mount | A signed FSKit extension mounts a virtual worktree on macOS | S | decision |
| 65 | The hub calls the remaining RPCs | The hub uses the RPCs it does not call yet (56 of 272 at the 2026-10-30 audit). **Check first**: recount | S | open |
| 66 | Hub insights rows recounted | The hub's insights rows that read `needs-both` are judged and counted again | S | open |
| 67 | Queue ETA | The landing card shows an estimated wait, not only the position | S | open |
| 68 | Monthly budget window | Agent budgets get a monthly window in `AgentBudgetLimits` | S | open |
| 69 | IDE plugins on the marketplaces | The VS Code, Open VSX and JetBrains plugins are published | S | decision |
| 70 | MSI from the public CI | The public release workflow builds the Windows MSI; today only the Forgejo flow does (`docs/RELEASE.md`) | S | open |
| 71 | Installer checks signatures on its own | `install.sh` verifies the minisign signature without `minisign` installed first. **Check first**: read `scripts/install.sh` | S | open |
| 72 | macOS test leg | CI runs the workspace tests on macOS. **Check first**: the `macos` job runs the `asc-vfs` tests and the `asc` unit tests only | S | open |
| 73 | Real-provider runs | Bring-your-own storage against AWS S3, and bring-your-own AI keys against a real provider (`TestKey`) | S | decision |
| 74 | Workflow template run for real | The TSRX check template (`asc workflow new`) runs once in CI against the real npm packages it names | S | open |

## 3. Order of work

1. Safety and the delta rule: 3, 6, 7, then 1 and 2 (28 and 32 are done).
2. Disk: 10, 14, 15.
3. Agents: 11, then 16, 17, 18, then 76 and 79.
4. CI reach: 19, 20, 23, 25, then 85.
5. Vulnerability awareness: 29, 31, 33, 36 to 40, then 30 and 34, then 86.
6. Later: 4, 8, 9, 12, 13, 21, 22, 26.
7. The hub: 46, 47, 48, 42, 43, 49, 52, then 44, 45, 50, 51, then 75, 80, 84.
8. Virtual file systems: 53, then 41.
9. Brought into scope: 57, 59, 70 to 72, 74, then 55, 56, 58, 54. Rows 61 to 63 and 65 to 68 follow in table order.
10. Server and accounts: 77, 82, 83, then 78.

Rows 64, 69 and 73, and the R2 half of row 85, wait for the owner (section 4).

## 4. Blocked on the owner

| Row | What is needed | Options |
|---|---|---|
| 64 | A Developer ID signing identity with the FSKit entitlement | (a) the owner provides the identity; (b) FSKit stays CI-built and ad hoc |
| 69 | Publisher accounts and tokens for VS Code Marketplace, Open VSX and JetBrains Marketplace | (a) the owner provides them; (b) the plugins stay release attachments |
| 73 | An AWS S3 bucket and a real AI provider key | (a) the owner provides them; (b) Garage stays the only real store tested |
| 85 | A Cloudflare R2 account for one recorded run | (a) the owner provides it; (b) only MinIO is added |

## 5. Decisions

| Date | Decision | Where |
|---|---|---|
| 2026-10-24 | gRPC-Web is served on the RPC listener | `docs/DECISIONS/0001-grpc-web-on-the-rpc-listener.md` |
| 2026-10-26 | The product is named ASC, Agent Source Control (naming table below) | this section |
| 2026-11-10 | One token system per server, chosen by `[auth] provider` | `docs/DECISIONS/0002-one-token-system-per-server.md` |
| 2026-11-12 | Full objects where speed matters: deltas are cold-storage compaction only | the principle above |
| 2026-11-14 | Web edits on signed branches are signed by the server with an instance key (row 81); the IDE plugins get a signing settings page (row 60); mirrors stored in asc (row 87) and in-place move to asc (row 88) are in scope | rows 60, 81, 87, 88 |
| 2026-11-13 | Row 5: deltas for cold history only | row 5 |
| 2026-11-13 | Row 16: first-class conflicts, design first | row 16 |
| 2026-11-13 | Row 22: `node_modules` snapshots, design first | row 22 |
| 2026-11-13 | Row 24: task runner as an optional module, design first | row 24 |
| 2026-11-13 | Row 25: keep the CI layout and cut the `projfs` job next | row 25 |
| 2026-11-13 | Row 26: read-only git view, design first | row 26 |
| 2026-11-13 | Rows 54 to 74 brought into scope from the SPEC and the status page | section 2 |
| 2026-11-13 | One tracking document (this file) and one out-of-scope review (`docs/OUT-OF-SCOPE.md`) | this file |

### Naming

The product is **ASC**, Agent Source Control: source control for people and their agents. The hub is **ASC Hub**. The rename is finished.

| Surface | Name |
|---|---|
| Command line tool and the one binary | `asc` (`asc.exe` on Windows) |
| Rust crates | `asc`, `asc-core`, `asc-store`, `asc-engine`, `asc-serve`, ... under `crates/asc*` |
| npm packages | `@asc/asc-client`, `@asc/asc-ui`, ... under `packages/asc-*` |
| Protocol package | `asc.v1` (directory `proto/asc/v1`) |
| Environment variables | `ASC_*` (for example `ASC_HOME`, `ASC_TOKEN`) |
| Per-repository files | `.asc/` (policy, projects, CI, review), `.ascignore`, `.ascattributes` |
| Home directory | the platform data directory under the name `asc`, overridden by `ASC_HOME` |
| Hidden refs | `refs/asc/` |
| Git configuration section | `asc.*` |
| Push options | `asc.ttl`, `asc.push.review` and the other `asc.*` options |
| Discovery document | `/.well-known/asc` |
| Container images | `asc-serve`, `asc-hub` |
| SQL objects | tables and columns prefixed `asc_`; the database role and schema are named `asc_hub` because `asc` is a reserved word in SQL |
| User agent | `asc/<version>` |
| Token prefixes | `asp_` (personal), `asd_` (deploy), `asc_` (CI) |

Stored-format identifiers carry the new name, so data written under an earlier name is not readable. The envelope magic is the two bytes `as`; the 4 byte magics are `ASC1` (git hook), `ASVS` and `ASVL` (vector files). Host names in committed files are placeholders under `example.org`.
