# Status: required versus implemented

Audit of 2026-10-30 at `origin/master` d1912ab2 (the last source change is 2451bf74; d1912ab2 touches tests only). It replaces the
2026-10-24 page. Method: every row was checked by reading code or tests, by grep over `crates/`, `proto/`, `.github/` and `D:/dev/asc-hub`,
or by `gh`; "docs say so" alone is not evidence. One test was run: `cargo test -p asc-core --lib transport_policy` (8 passed).
The full workspace was not rebuilt here; CI is the gate for that.

Words: **done** = code and a test on `origin/master`; **partial** = works but a named part is missing, unenforced or unverified;
**missing** = absent on `origin/master`. Work in flight on a branch is named, not counted as done.

## Headline

- Server surface: 33 gRPC services, 272 RPCs. Every RPC has a handler (tonic requires it). The hub calls 216 of them from non-test Go
  code. No `TODO`, `FIXME`, `todo!()` or `unimplemented!()` exists in `crates/`, `web/`, `packages/`, `plugins/`, `deploy/`, `scripts/`.
- CI (GitHub, `Jordan-Hall/asc`): last 60 runs on master: `image` 27 success; `ci` 6 success, 9 failure, 14 cancelled (superseded). The last
  completed `ci` (c0b6bbd3) is green; the run for HEAD is in progress. `ci.yml` runs Rust fmt, clippy `-D warnings` and tests on Linux only.
- Release: signing key and public key are now set (`gh secret list`, `gh variable list`), but no tag, no release and no `release` workflow
  run exists. The release chain has never run end to end.
- Not merged: packed objects and import batching sit on `feat/storage` (4 commits, 19 behind master).

## Requirements

### User requirements

| Requirement | Source | Status | Evidence | Gap |
|---|---|---|---|---|
| Tokens: hub personal access tokens, one token system | FEATURE-ROADMAP "Rules" | done (hub mode) / partial (standalone) | `crates/asc-serve/src/forgejo.rs:728` `decide_scoped` asks the hub per credential and honours the PAT scope (probe at `:824`); hub side `routers/private/asc_parity_test.go` | Standalone `token_store` mode (`authz/token_store.rs`) is a second token system (`asc p_<id>_<secret>`, grants by glob). Intended for no-hub installs, but the roadmap says "never a second token system"; decide and document |
| Signed forwarded claim | BUILDOUT s.16, SPEC s.15 | done | `crates/asc-rpc/src/forwarded.rs` (5 tests), `asc-serve/src/authz/claim.rs:43` accepts a claim only from the host service and for the exact subject; hub signs in `services/ascidentity/ascidentity.go` (`asc-forwarded-v1`) | Replay window is a clock-skew bound, not a nonce; fine for the stated threat model |
| Row-level visibility equals the platform | FEATURE-ROADMAP DoD 5 | partial | hub-mode decisions come from the hub (`forgejo.rs:800` anonymous 200 = public); `NOT_FOUND` for unreadable scopes (`docs/AUDIT.md`); `tests/cross_search.rs`, `tests/codeintel_acl.rs`; hub parity matrix test | No asc-side parity matrix over every list/search RPC; `TokenStoreAuthz` has no anonymous or public-repo notion (`authz/token_store.rs:1016`, `token.ok_or(Unauthenticated)`), so a standalone server cannot serve a public clone without a token |
| CLI auth per repo, on demand, like git | FEATURE-ROADMAP "CLI authentication model" | done | `crates/asc/src/remote/demand.rs:81` `acquire`; `tests/auth_demand.rs`: anonymous public clone, private prompts then retries and reuses, helper credential, stale credential dropped, no-access gives one clean error, push and fetch on 401, pasted token never echoed | Credential store is git helpers only; `feat/cli-keystore` (uncommitted, `D:/tmp/asc-kc`) adds an OS keystore. Path-scoped storage only with `credential.useHttpPath` (`credential.rs:802`) |
| Never prompt in CI; no required login | same | done / partial | `credential.rs:461` `interactive_with`: needs stdin and stderr TTY; `GIT_TERMINAL_PROMPT=0` forces off; test `a_private_clone_without_a_terminal_never_asks...` names `ASC_TOKEN` and credential helper; `asc auth login` optional | `CI=true` is not read: a CI job that has a pty would prompt, and `ASC_AUTH_INTERACTIVE=1` overrides. Add a `CI` check |
| SSH or HTTPS only; no plain http | memory, SPEC s.15.6 | done | `asc-core/src/transport_policy.rs:191` `classify` (8 tests pass); used by 15 client files; loopback http only by opt-in `ASC_ALLOW_LOOPBACK_HTTP` | The container serves plain http on 7890 behind a TLS proxy by design (`Dockerfile.serve`); HTTP/3 needs `[tls]`. SSH on staging not deployed |
| Forge-neutral client | memory, WEB-UI s.1 | done | no Forgejo or Gitea name in `crates/asc/src` except the `--forgejo-reads` alias in `cli/serve.rs:155` and `GITHUB_*` env readers in `cli/ci/checkout.rs`; device flow is plain RFC 8628 (`cli/auth_device.rs`) | `remote/url.rs:32` has a stale comment ("stubbed to git until U50") |
| TSRX in code intelligence | FEATURE-ROADMAP TSRX table, BUILDOUT s.17 | done | `asc-codeintel/src/tsrx.rs`, `tags/tsrx.rs`, golden `tests/fixtures/tags/sample.tsrx`, `tsrx/tests.rs`; imports `serve/deps/imports.rs`; hunk scope `serve/review/hunk.rs` | Only server-side |
| TSRX syntax highlighting, fast | FEATURE-ROADMAP TSRX table | partial | Hub: chroma lexer `modules/highlight/tsrx` with a linear-time budget test `perf_test.go` | asc has no highlighter: `web/src/routes/blob.tsx` shows plain lines; `plugins/vscode/package.json` registers no language or grammar for `.tsrx`; JetBrains not checked. No proto field carries highlight tokens |
| TSRX tooling docs (Prettier or ESLint plugins in workflow templates, language server) | FEATURE-ROADMAP TSRX table | missing | grep `tsrx` in `docs/` matches only SPEC and BUILDOUT | Write the doc and a workflow template |
| BYO S3-compatible storage | BYOS.md, BUILDOUT s.9 | partial | `asc-serve/src/storage/` (12 RPCs, 61 tests, in-process S3 `fake_s3.rs`), vault `vault/` (24 tests), SSRF guard `netguard.rs` | Never run against real Garage, S3 or Postgres; `docker-compose.it.yml` uses MinIO and `ci.yml` does not start it. No usage-by-kind RPC, no migration pause (hub rows RepoSettingsStorage) |
| BYO AI keys | BUILDOUT s.10, s.31 | partial | `asc-serve/src/ai/` (AiProviders 8 RPCs, AiKeyShares 7, 49+ tests, real provider code in `ai/provider.rs`, fake only under `#[cfg(test)]`) | Never run against a real provider. `aikeyshares.v1` is now advertised with `ai-keys.v1` (`agents/register.rs` `features_for`, test `feature_tests`; branch `feat/aikeyshares-feature`). No AI feature routing or per-feature cap |
| Packed objects | docs/design/PACKED-OBJECTS.md | partial (unmerged) | On `feat/storage`: `asc-store/src/pack/` (format, SQL-free bucket index, layer, job, 802-line tests), `asc-serve/src/pack_service.rs`, migration 0596, `asc serve admin pack` | Not on master; `origin/master` has no `pack` module. Feature is off by default per the design; rollout phases 2 and 3 not started. Open questions (Derived values, `max_object`) unanswered |
| Import batching | same, "Import: batches" | partial (unmerged) | `feat/storage` bba5003e: `asc-git/src/import.rs`, `asc-store/src/batch.rs`, `tests/import_batch.rs`; bench 220 s to 93 s in `docs/BENCHMARKS.md` on that branch | Not on master. No import progress source for the hub (row NewRepo "Import running") |
| No TODOs or stubs | FEATURE-ROADMAP DoD 2 | done in asc, partial in docs | zero hits (see Headline). Production "unimplemented" statuses are conditional (`later("InterDiff"/"RestackChanges"/"RequestMerge")` at `changes/rpc.rs:831`, wired by `interdiff.rs:69`, `restack.rs:254`, `request_merge.rs:193`) | `[stacks] cleanup_days` is now enforced by the landed-ref reaper (`reftl.rs`, tests in `tests/reftl.rs`; branch `feat/stacks-cleanup-days`). `restack_on_main_move` is recorded and returned and the host acts on it by design (BUILDOUT s.25): a restack rewrites a person's refs, so the server does not do it unasked `docs/PLAN-STATUS.md` is stale (dated 2026-10-25, hub-side rows) |

### Hub integration (every ascv1 RPC and field the hub uses)

| Requirement | Source | Status | Evidence | Gap |
|---|---|---|---|---|
| Each RPC the hub calls is served | `D:/dev/asc-hub/modules/ascproto/ascv1`, `modules/ascclient` | done | script over 33 services: 216 of 272 RPCs called by the hub, 0 without a handler; 32 services mounted (`*Server::new` in `asc-serve`, `Bridge` is client-side) | The 56 RPCs the hub does not call are hub work: Review `ListThreads`, `AddComment`, `ResolveThread`, `RequestReview`, `SetBrief`, `SuggestReviewers`, `PrepareSuggestion`, `HunkContext`, `DraftContext`; Workspace 11; Ci 5; Projects 3; Repo `Ingest`, `Rebase`, `WatchRefs` and others |
| Feature strings the hub gates on | `asctrust`, `ascclient.CheckServer` | partial | `Version.features` built at `asc-serve/src/lib.rs:755` | `aikeyshares.v1` now advertised (`feat/aikeyshares-feature`); `stacks.undo.v1`, `ai-keys.v1`, `mergequeue.v1` etc. are advertised |
| Stats for the hub's "who writes the code", insights | feature-gaps repo.md Insights rows | done | `Insights` service, `review/` gate stats, `0a9ee9dc` ref-log counts | Hub rows still read `needs-both` at 2026-10-28; recount |
| Review brief, suggestions, approvals, dismissal, overrides, queue | BUILDOUT s.19, s.20, s.22 | done | `asc-serve/src/review/` (38 tests), `queue/` (14), commits 71a18790, ed5c0e89, 8a220c57 | Queue ETA only has position (hub row Review "Landing card") |
| Agent control, pause, checkpoints, plan | BUILDOUT s.18 | done | `PauseRun`, checkpoint in `agents.proto`; `agents/` 93 tests | Hub row "monthly budget per token" has no monthly window in `AgentBudgetLimits` |
| Policy keys of the Policy board | BUILDOUT s.25 | partial | `asc-core/src/policy.rs`, `policy/rpc.rs:166` | `cleanup_days` enforced (above); `restack_on_main_move` is host-driven; `Strategy` (`asc-core/src/policy.rs:507`) has no squash variant (rebase, rebase-disjoint, fast-forward); hub rule "per-ref kinds" absent |
| Workflow secret scopes, variables, log redaction | feature-gaps repo.md RepoSettingsSecrets | partial | `workflows/secrets.rs`, `agents/mask.rs` mask secrets | `workflows.proto` secret RPCs carry name and value only; no scope chips, no variables table |
| Code search "why it matched" | feature-gaps repo.md Search | missing | `codeintel.proto:136` `Chunk` has no reason | Add field and fill from retrieval source |
| Deps scan progress | feature-gaps repo.md Dependencies | missing | `deps.proto:19` `Scan` is unary | Add status or stream |
| Storage usage by kind; migration pause | feature-gaps repo.md RepoSettingsStorage | missing | `storage.proto` has no usage RPC; only start, get, list, cancel migration | Add |
| Backups, usage, org settings, sessions, data export | BUILDOUT s.26-s.33 | done | `backups/` 28 tests, `usage/` 9, `org/` 8, `export/` 4, `sessions/` 3 | Thin tests on sessions, export, usage, org, inbox (6), releases (8) |
| Other `needs-asc-backend` rows (about 110 of 126 listed) | feature-gaps/*.md | partial | spot checks above | The files are dated 2026-10-28, before BUILDOUT s.16-s.33 landed; the counts in `FEATURE-ROADMAP.md` are all zero. Re-run the audit before planning from the rows |

### Specification and git compatibility

| Requirement | Source | Status | Evidence | Gap |
|---|---|---|---|---|
| Local client verbs, git shim | SPEC, CLI.md | done | all 130 command names in CLI.md occur in `crates/asc/src`; `tests/` has about 100 files; compat suite against pinned git 2.55.0 (`docs/git-fidelity.md`) | |
| Git "not supported" list | grep `not supported` | partial | octopus merge `cli/merge.rs:278` (in flight on `feat/stubs`, `D:/tmp/stubs`); rebase of a root commit `rebase.rs:1084`; rebase and cherry-pick on unborn branch `rebase.rs:673`, `pick.rs:323`; `push --mirror` `push.rs:135`; interactive `pull --rebase` `pull.rs:94`; `log --graph` with a pathspec `log/walk.rs:300`; `grep` back-references `grep.rs:270`; `read-tree` of several trees `write_tree.rs:149`; file/dir and case conflicts `conflict.rs:495`; `worktree convert` `ext.rs:162`; tagging a tag `tag.rs:624`; deleting a symbolic ref `symbolic_ref.rs:194` | By design: LFS, `submodule update`, IDE git panels |
| Divergences | `docs/divergences.md` | partial | 14 listed | D-5, D-6 "pending B12" (server policy for tags and `push --all`); D-7 hook via `--absolute-git-dir` open |
| Audit log, undo | `docs/AUDIT.md` | done | `audit/` 97 tests | |
| Protocol hardening, MCP, plugin API | `docs/mcp.md`, `plugin-api*.md` | done | `asc-mcp`, `serve/mcp/` 28 tests, `tests/mcp_*.rs` | Inspector run only on Forgejo (`mcp-inspector.yml`) |
| Web UI | `docs/WEB-UI.md` | partial | `web/` with Playwright smoke suite (1db94c8f) | Last commits say "wip"; no syntax highlight; not run in GitHub CI; now embedded in the image and the release: `deploy/Dockerfile.serve` builds `web/dist` in a first stage and `release.yml` job `web` feeds every platform build; `ASC_REQUIRE_WEB=1` (`asc-serve/build.rs`) makes a missing bundle a build error (branch `feat/web-bundle-image`; the image build on its pull request is the end-to-end check) |
| IDE plugins | CLI.md s.10 | partial | `plugins/vscode`, `plugins/jetbrains`, release jobs in `release.yml:171` | Never published; no TSRX language |

### Release, install, deploy, CI

| Requirement | Source | Status | Evidence | Gap |
|---|---|---|---|---|
| Release workflow (5 platforms, SBOM, minisign, manifests) | `.github/workflows/release.yml`, RELEASE.md | partial | 342 lines; rehearsal via `workflow_dispatch`; requires `ASC_RELEASE_PUBKEY` (set 10:40) and `ASC_MINISIGN_KEY` (set 11:04) | Never run: no tag, no release, no rehearsal run in `gh run list -w release`. Windows MSI and macOS legs unproven; Windows runner not registered (Forgejo side) |
| `install.sh` | `packaging/linux/install.sh` | partial | verifies minisign before unpacking (`:28-31`); `release-scripts.yml` tests installers | Needs `minisign` pre-installed; uncommitted edits in the main checkout; no release to install |
| Self-update | `cli/upgrade.rs:129` `verify`, `tests/self_update.rs` | done | minisign-verify with a compiled-in key, no key means refusal (`:506`) | Untested against a real release |
| `deploy/Dockerfile.serve` | SPEC s.8.1 | partial | pinned digests, non-root, healthcheck `/healthz`; `image.yml` 27 successes | Web bundle added in `feat/web-bundle-image`; image is built but a staging deploy of this build is not recorded |
| CI | `ci.yml`, `image.yml` | partial | Rust gate green at c0b6bbd3 | No TS job (web, `packages/*`, plugins), no Windows or macOS test leg, no Postgres, no S3 integration, no fuzz or bench (those live in `.forgejo/workflows`, runner-dependent). Flaky port-race tests recur (6617bcd9, d1912ab2) |
| Staging deploy | `deploy/coolify`, ROADMAP release cadence | missing | none recorded this session | Needs a release, Coolify token |

## Prioritized work units

Each unit names files and one acceptance command. Order: unblock hub first, then correctness gaps, then packaging.

1. **Merge packed objects and import batching.** Rebase `feat/storage` on master (19 behind), review, fast-forward. Files: `crates/asc-store/src/pack/*`, `crates/asc-serve/src/{pack_service,pack_sql,pack_tests,registry,gc,config}.rs`, `crates/asc-git/src/import.rs`. Check: `cargo test -p asc-store -p asc-git -p asc-serve --lib` and `cargo test -p asc-git --test import_batch`.
2. **Advertise `aikeyshares.v1`.** (done, `feat/aikeyshares-feature`) Push the feature when `AiKeyShares` is mounted. Files: `crates/asc-serve/src/ai/mod.rs`, `crates/asc-serve/src/lib.rs`. Check: a test in `crates/asc-serve/tests` that `Version.features` contains `aikeyshares.v1`.
3. **Enforce `[stacks] cleanup_days` (done, `feat/stacks-cleanup-days`); `restack_on_main_move` stays host-driven.** Files: `crates/asc-core/src/policy.rs`, `crates/asc-serve/src/{expire,gc,policy/rpc,changes/restack}.rs`. Check: `cargo test -p asc-serve --lib stacks_policy` with a test that a stack older than `cleanup_days` is cleaned and a main move restacks.
4. **Embed the web bundle in the image and release.** (done, `feat/web-bundle-image`) Add a bun build stage to `deploy/Dockerfile.serve` and a build step to `release.yml`/`image.yml` so `web/dist` exists at compile time. Check: `docker build -f deploy/Dockerfile.serve .` then `curl /ui/` returns 200 with `[web] enabled=true`.
5. **First release rehearsal.** Run `release.yml` by `workflow_dispatch`, fix what breaks, then tag. Files: `.github/workflows/release.yml`, `packaging/*`. Check: `gh run view` green and `packaging/linux/install.sh` installs the rehearsal artefact in a clean container.
6. **Add `CI` to the prompt gate.** Files: `crates/asc/src/credential.rs`, `crates/asc/tests/auth_demand.rs`. Check: `cargo test -p asc --test auth_demand` including a case with `CI=true` and a pty-like override.
7. **Real-service integration job.** Start Garage or MinIO and Postgres in CI; run storage, migration and pack tests against them. Files: `.github/workflows/ci.yml`, `docker-compose.it.yml`, `crates/asc-serve/tests/`. Check: the job is green on master.
8. **TSRX highlighting in asc.** A tree-sitter-free linear tokenizer for `.tsrx` in the web blob view and a TextMate grammar in the VS Code plugin. Files: `web/src/routes/blob.tsx`, `web/src/lib/highlight/*`, `plugins/vscode/package.json`, `plugins/vscode/syntaxes/tsrx.tmLanguage.json`. Check: `bun test` in `web` with a golden and a linear-time budget test like the hub's `perf_test.go`.
9. **TSRX tooling doc and workflow template.** Files: `docs/CONFIG.md` or a new `docs/TSRX.md`, a template under `crates/asc-serve/src/workflows/`. Check: workflow template validates (`cargo test -p asc-serve --lib workflows`).
10. **Standalone mode public repositories.** Let `TokenStoreAuthz` serve repositories marked public to anonymous callers, with `NOT_FOUND` for private. Files: `crates/asc-serve/src/authz/token_store.rs`, `registry.rs`, `proto/asc/v1/repo.proto`. Check: `cargo test -p asc-serve --test token_store` plus an anonymous clone test.
11. **Hub-facing proto gaps.** `Chunk.reason`, deps scan progress, storage usage by kind, migration pause, workflow secret scopes. Files: `proto/asc/v1/{codeintel,deps,storage,workflows}.proto` and handlers. Check: per-RPC tests with a caller lacking the right and another tenant.
12. **asc-side parity matrix.** One test table: every list and search RPC against read, write, admin, claim and anonymous callers. Files: `crates/asc-serve/tests/parity.rs`. Check: `cargo test -p asc-serve --test parity`.
13. **CI breadth.** Add a TS job (`bun test`, oxlint, typecheck for `web`, `packages/*`), a Windows test leg, and mark port-race tests retry-safe. Files: `.github/workflows/ci.yml`. Check: workflow green.
14. **Git gaps.** Finish octopus merge (`feat/stubs`), root-commit rebase, unborn rebase and cherry-pick, `push --mirror`, then D-5 and D-6 server policy. Files: `crates/asc/src/cli/{merge,rebase,pick,push}.rs`, `docs/divergences.md`. Check: `cargo test -p asc --test compat` and each new `cli_*` test.
15. **Test depth for thin modules.** Sessions (3), export (4), inbox (6), releases (8), org (8), usage (9): right caller, wrong caller, other tenant, retry. Files: the module test files. Check: `cargo test -p asc-serve --lib <module>`.
16. **Refresh the docs.** `PLAN-STATUS.md` (stale, hub-side), `remote/url.rs:32` comment, the hub's `FEATURE-ROADMAP.md` counts. Check: grep finds no "stubbed to git until U50".
17. **Run BYOS and BYO AI against real providers once** and record results in `docs/BYOS.md`. Check: `asc doctor --store` against the staging Garage bucket and `TestKey` against a real key.

## In flight (not counted above)

`feat/storage` (packs, batches), `feat/cli-keystore` (OS keystore, uncommitted), `feat/stubs` (octopus merge, uncommitted), `feat/cli-auth`
(merged into master history), `feat/agent-activity-days` (1 commit ahead), `D:/tmp/feat-asc-b` (settings CLI edits, uncommitted).
