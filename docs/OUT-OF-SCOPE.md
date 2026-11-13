# Out-of-scope review

Written 2026-11-13. Every item that a document puts out of scope, defers or calls "not built" is listed once here, with a verdict. Open work goes to `docs/ROADMAP.md`; this file only records the judgement.

Each item was judged against the goals: smaller files and storage, faster operations, worktrees without duplicated bytes, faster CI/CD, wide adoption, forge-neutral clients, https and ssh only, and bring-your-own keys and storage.

**Verdicts:**

- **bring in**: a new roadmap row, or an existing row named.
- **already on the roadmap**: a row already covers it.
- **already built**: the code exists; the stale statement in the source document was fixed (or is listed as not editable).
- **stays out**: a concrete reason is given.

Rows 54 to 74 of the roadmap come from the review dated 2026-11-13 and stand as written. They are referenced below, not repeated.

Sources searched: `docs/SPEC.md` (including "Not built in v1" and section 25.11.1), `docs/BUILDOUT.md`, `docs/history/PLAN.md`, `docs/divergences.md`, `docs/adapter-gaps.md`, `docs/git-defects.md`, `docs/CARRY.md`, `docs/WORKTREES.md`, `docs/BYOS.md`, `docs/DEPENDENCIES.md`, `README.md`, the merged gap lists, and the hub's `docs/asc` (read only).

## Storage and the data model

| Item | Where stated | Verdict | Reason | Roadmap row |
|---|---|---|---|---|
| Object dedup across a fork network | SPEC §0 "Not built in v1"; §3.3; §19 decision 6 | already on the roadmap | Smaller storage | 8 |
| Shared `BucketRefs` (multi-writer refs on a bucket) | SPEC §3.4 ("Deferred, unit U30"); §19 Ops row | already built | `crates/asc-store/src/bucketrefs.rs`, `tests/it_bucketrefs.rs`, `--refs shared`. SPEC §3.4 and the Ops row were fixed | 8 (fork networks only) |
| Segment packing and zstd dictionaries | SPEC §0 "Not built in v1" | already built | `docs/design/PACKED-OBJECTS.md`; on by default is a separate question | 63 |
| A tree-format change | SPEC §0 principle 3 | stays out | Changeset ids hash the tree, so a format change rewrites all history | |
| Cold-only deltas | SPEC §0; roadmap principle | already on the roadmap | Decided 2026-11-13 | 5 |
| MinIO and R2 `If-None-Match` behaviour not measured | SPEC §19 spikes, S3 row | bring in | Bring-your-own storage needs the common stores tested. MinIO runs in CI; R2 needs the owner's account | 85 |
| AWS S3 and a real AI provider | `docs/BYOS.md` "Real-provider runs" | already on the roadmap | Needs the owner's accounts | 73 |
| `[pack] enabled` off by default | `docs/design/PACKED-OBJECTS.md` rollout | already on the roadmap | Smaller storage after a measurement | 63 |
| Hub storage IDs as asc ids (plan unit K16, "Stage 6 IDs") | `docs/history/PLAN.md` K16; SPEC §14.3 step 7 | stays out | Imported changesets keep their git SHAs, so the hub's id columns already work. Switching every id column is a large migration with no gain for any goal | |
| Folding a second repository into the monorepo | SPEC §14.3 step 7 | stays out | An operator migration of one repository, not product work. The importer already handles it | |
| Artefacts stored in asc | SPEC §14.3 step 7 | already built | Unit U75, `asc ci artifact` (`crates/asc/src/cli/ci/artifact.rs`). SPEC §14.3 was fixed | |

## Git compatibility

| Item | Where stated | Verdict | Reason | Roadmap row |
|---|---|---|---|---|
| `shortlog`, `name-rev`, `whatchanged`, `range-diff`, `cherry`, `notes show` | SPEC §11.5 table; §0 "Not built in v1" | already built | Unit U24f, `crates/asc/src/cli/{shortlog,name_rev,whatchanged,range_diff,cherry,notes}.rs`. SPEC §11.5 and §0 were fixed | 59 (check and close) |
| A full `.git` projection, git's index as the staging area, importing commits made by real git | SPEC v1.1 decision (3) | stays out | Writes from real git would make a second source of truth. The tools that only read git are covered by row 26 | 26 |
| The IDEs' built-in git | SPEC v1.1 decision (3); §0 | already on the roadmap | Needs the read-only git view | 26 |
| `git-remote-asc` helper (plan unit U38) | SPEC §9.6; `docs/history/PLAN.md` U38 | stays out | Git clients already reach an asc server over smart HTTP and SSH (SPEC §25) | |
| `filter-branch`, `filter-repo`, `replace` | SPEC §11.5 table | stays out | They rewrite history; changeset ids hash content, so a rewrite changes every id downstream | |
| `svn`, `p4`, `send-email` | SPEC §11.5 table | stays out | Real git runs them outside an asc workspace (SPEC §11.4); no goal needs them inside | |
| `daemon`, `instaweb` | SPEC §11.5 table | stays out | Both serve plain, unauthenticated http, which breaks the https and ssh only rule | |
| `notes` (write) | SPEC §11.5 table | stays out | The data model has no notes store; review threads and metadata live on changes | |
| `ident`, `working-tree-encoding` and `filter=<x>` attributes | SPEC §2.4.1 | stays out | They change bytes at checkout, which breaks virtual file sizes and turbo's stat hashing | |
| `text`, `eol` and `crlf` not applied in virtual worktrees | SPEC §2.4.1 | stays out | Same reason: virtual files hold the committed bytes so sizes come from the tree | |
| Git LFS (client, server API, hub, IDE plugins) | SPEC §2.5, §24.3.5, §25.1; hub `WHY.md` | stays out | Large files are native and chunked; LFS would store each file twice and add a second transfer path | |
| The hub's web LFS settings pages | hub `asc-stage4-writer.md` "Not done yet" | stays out | No LFS (row above) | |
| `submodule update` and `init` | SPEC §10.6, §11.5 table | stays out | Pins are native and the CLI has writable mounts; the commands print a notice | |
| Tags whose target is not a commit | SPEC §14.1 step 6 | stays out | `TagRecord` targets commits; such tags are very rare and the importer reports them | |
| Signed pushes (`--signed`) | SPEC §25.1 | stays out | Commits can be signed; push certificates need a nonce store and almost no host uses them | |
| The dumb HTTP protocol | SPEC §25.1 | stays out | Slow, and every git client speaks smart HTTP | |
| `git-upload-archive`, `bundle-uri`, `object-info`, SHA-256 object format | SPEC §25.1 | stays out | `Archive` covers archives; the other two are optimisations that no client needs; asc ids map one to one to SHA-1 git ids | |
| An SSH shell, other SSH exec commands, forwarding | SPEC §25.1 | stays out | Security: the listener runs the two pack commands only | |
| Automatic restack after a git rewrite (plan unit C16) | SPEC §20.1 D4; `docs/history/PLAN.md` C16 | stays out | D4 promises that no git command changes behaviour; `asc restack` does it on request | |
| Divergence Q-1 (alias before an external command) | `docs/divergences.md` | stays out | Idealized on purpose; recorded in the compat allow list | |
| Divergences Q-3, Q-6, Q-7, Q-8 | `docs/divergences.md` | stays out | Each is a refusal that keeps the store or the credential safe | |
| Git defects U-1 to U-4 | `docs/git-defects.md`; `docs/divergences.md` | stays out | Git's behaviour is a defect (header leak, unsafe patch, broken abort, lost file); asc refuses | |
| Own divergences D-1 to D-6 | `docs/divergences.md` | stays out | Proven by tests and required by the synthetic gitdir, the credential flow or branchless mode. D-7 is retired | |
| Adapter gaps A-1 to A-6 | `docs/adapter-gaps.md` | already built | All six are closed | |

## Worktrees and the daemon

| Item | Where stated | Verdict | Reason | Roadmap row |
|---|---|---|---|---|
| Workspace sync | SPEC §0 "Not built in v1" | already on the roadmap | Same item as snapshot sync | 12 |
| Host-level fetch proxy for CI | SPEC §9.1; §0 "Not built in v1" | already on the roadmap | Gated on a measurement | 58 |
| Virtual CI checkout on Windows and macOS | SPEC §12.3 ("Linux runners only") | already on the roadmap | Faster CI on every runner | 23 |
| A Cloud Files folder cannot be renamed while registered | `docs/WORKTREES.md` | stays out | A Windows API limit; the worktree is moved by unregistering first | |
| Link mode | `docs/WORKTREES.md` | already built | Documented as the last resort | 15 |
| A kernel driver of our own | roadmap row 41 | stays out | Needs driver signing on Windows and macOS | 41 |
| Daemon-side turbo cache tier | SPEC §0 ("gated on measurement") | already built | `crates/asc/src/cli/cache/upstream.rs`, `front.rs` | |
| Path log beyond the mainline index | SPEC §0 | already built | Derived type `pathlog.v1` | |

## Agents, MCP and the IDE plugins

| Item | Where stated | Verdict | Reason | Roadmap row |
|---|---|---|---|---|
| First-class conflicts | SPEC §0 | already on the roadmap | Decided 2026-11-13 | 16 |
| `WatchRefs` | SPEC §0 "Not built in v1" | already built | `crates/asc-serve/src/watch_refs.rs`; the daemon uses it. SPEC §0 was fixed. The row covers callers that still poll | 18 |
| Pause at the AI cap (`stop_at_cap`) | merged gap list (agents) | bring in | Today the run ends; a pause keeps the agent's work | 76 |
| Person profile tool for agents | merged gap list (review) | bring in | `Agents.GetPerson` exists; the tool lists lack it | 79 |
| MCP resource subscriptions on a remote server | SPEC §22.3.5 ("L only in v1") | stays out | A subscription needs a long-lived stream per client; `WatchRefs` already pushes ref moves | 18 |
| OAuth dynamic client registration | SPEC §22.7.3 | stays out | Deprecated in 2026-07-28; pre-registered public clients with PKCE cover the need | |
| The forge's own MCP (plan units M20 to M28) | `docs/history/PLAN.md`; SPEC §22 (M-D11) | stays out | MCP is served by `asc serve` for any forge, which keeps clients forge-neutral | |
| AI commit messages and AI compose in the plugins | SPEC §24.3.5 (IP-D12) | stays out | Decided in IP-D12: agents reach asc through MCP; the plugins stay free of AI | |
| Cloud workspaces shared across a team | SPEC §24.3.5 | already on the roadmap | Snapshot sync and the commit-cloud view cover it | 12, 45 |
| GitHub, GitLab and Jira integrations in the plugins | SPEC §24.3.5 | stays out | The plugins talk to asc only, which keeps them forge-neutral | |
| git-flow | SPEC §24.3.5 | stays out | Stacks replace it | |
| Submodule editing in the plugins | SPEC §24.3.5 | stays out | The CLI's writable mounts do it (SPEC §10.6) | |
| Paid tiers | SPEC §24.3.5 | stays out | There is no billing in the product | |
| A signing settings page in the plugins | SPEC §24.3.5; §24 signing row | already on the roadmap | Needs the owner's decision | 60 |
| Command palette, repo tabs, profiles, work items in the plugins | SPEC §24.3.5 | stays out | The IDEs have their own; work items are the hub's issues | |
| Marketplace publishing | README; release notes | already on the roadmap | Needs the owner's tokens | 69 |
| "IDE plugins are in progress" | `README.md` "Status" | already built | Built, signed and attached to `v0.1.4`. README was fixed | |

## CI, build tools and dependencies

| Item | Where stated | Verdict | Reason | Roadmap row |
|---|---|---|---|---|
| `node_modules` snapshots | SPEC §0 | already on the roadmap | Decided 2026-11-13 | 22 |
| Task runner `asc run` | SPEC §0 | already on the roadmap | Decided 2026-11-13 | 24 |
| TypeScript import-graph inference | SPEC §0 "Not built in v1" | already on the roadmap | | 56 |
| Lockfile parsing for the project graph | SPEC §0, §12A.2 | already on the roadmap | | 57 |
| Merge queue with speculative CI | SPEC §12.6 | already on the roadmap | Status `skip`: post-merge testing by decision | 35 |
| File locking for binary files | roadmap | already on the roadmap | Status `skip` | 27 |
| GIT ranges never match in an advisory file | `docs/DEPENDENCIES.md` "Vulnerabilities" | stays out | Evaluating a GIT range needs the upstream repository's history; the OSV API path evaluates them | |
| CocoaPods has no advisory data | `docs/DEPENDENCIES.md` | stays out | OSV has no CocoaPods source to match against | |
| Scan progress held in memory | `docs/DEPENDENCIES.md` "Progress" | bring in | A restart or a load balancer loses it; servers behind a balancer are common | 86 |
| Go module zips over 128 MiB are not verified | `docs/DEPENDENCIES.md` "Server config" | stays out | The limit is a setting (`archive_max_bytes`); an operator can raise it | |
| MSI only from the Forgejo flow | `docs/RELEASE.md` | already on the roadmap | | 70 |
| Installer needs `minisign` | `scripts/install.sh` | already on the roadmap | | 71 |
| macOS runs part of the tests | `ci.yml` | already on the roadmap | | 72 |

## The server and the hub

| Item | Where stated | Verdict | Reason | Roadmap row |
|---|---|---|---|---|
| asc RPC over SSH, SSH certificates, `sk-*` keys | SPEC §25.11.1 "Not in v1" | already on the roadmap | | 54, 55 |
| OpenSSH forced-command entry | SPEC §25.11.1 "Not in v1"; §25.11.2 alternatives | bring in | Lets an operator keep `sshd` in front; wide adoption | 83 |
| Suggestions, the review brief, carry-over, agent controls, spend, gate statistics, the merge queue, settings rows | BUILDOUT §16 "Not built yet" | already built | `asc-serve/src/review/`, `queue/`, `agents/`. BUILDOUT was fixed | |
| Monthly budget window | BUILDOUT §16; hub row | already on the roadmap | | 68 |
| Queue ETA | hub Review "Landing card" | already on the roadmap | | 67 |
| Administrator read of one person's usage | merged gap list (accounts) | bring in | Needed to run a shared server | 77 |
| Live restore | merged gap list (backups) | bring in | A restore today needs downtime and a restart | 78 |
| CLI for the account services | merged gap list | bring in | A standalone server without the hub has no client for them; forge-neutral | 82 |
| Teams as an asc concept | merged gap list | stays out | Teams belong to the forge; asc sees owner and repository grants | |
| Hub integration matrix over both backends (plan unit K15) | `docs/history/PLAN.md` K15; hub `asc-stage4-writer.md` | bring in | The hub's tests must prove both storage backends | 75 |
| Migration of git repositories into asc | hub `WHY.md` (lines 38, 50), `ARCHITECTURE.md`, `ASC-NATIVE-UI.md`, `asc-stage4-writer.md` | already built | Hub `services/repository/migrate_asc.go`, `docs/asc/MIGRATION-IMPORT.md`. The hub documents are read only here and stay stale | |
| A dedicated audit page | hub `WHY.md` line 50 | already built | Hub `templates/repo/asc_audit.tmpl`, with admin and org pages. Read only here | |
| Pull mirrors and wiki pull mirrors into asc | hub `asc-stage4-writer.md` "Not done yet" | bring in | Mirrors are a common way to adopt a new host | 80 |
| Signed asc commits from web edits | hub `WHY.md`; `asc-stage4-writer.md` | bring in | Needs the owner's decision on who signs | 81 |
| Issue counts per state and due dates | hub `UI-COVERAGE.md` | bring in | Board fidelity | 84 |
| Private, Internal and Public visibility | hub `UI-COVERAGE.md` | stays out | The hub has one private flag; asc follows the platform's visibility. A third level changes the upstream forge's data model | |
| The hub calls the remaining RPCs | status audit | already on the roadmap | | 65 |
| API and webhook reference page | roadmap | already built | Hub page `/-/api` | 49 |

## Decision rows

These rows need the owner. The questions and options are in `docs/ROADMAP.md` section 4.

| Row | Question |
|---|---|
| 60 | Do the IDE plugins get a signing settings page, or does `asc config` stay the only way? |
| 64 | Will the owner provide a Developer ID identity for a real FSKit mount? |
| 69 | Will the owner provide marketplace publisher tokens? |
| 73 | Will the owner provide an AWS S3 bucket and a real AI provider key? |
| 81 | Do web edits on signature-required branches get signed by an instance key, get refused, or stay out? |
