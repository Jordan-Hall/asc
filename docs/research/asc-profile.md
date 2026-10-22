# ASC monorepo workload profile (target for asc)

Measured 2026-10-22 on the developer workstation (Windows 11, NTFS, D: 1.4 TB at 90% used, 144 GB free).
Repository: `D:\dev\monorepo` (Forgejo `asc/dds`), HEAD `318ce5b8` (2026-10-20). All measurements were
read-only (`git ls-tree`, `git rev-list`, `git cat-file --batch-check`, `du`, `find`, `stat`, `fsutil hardlink list`).
Nothing was pruned, gc'd or removed.

Sizes are binary units (KiB/MiB/GiB) unless stated. "Apparent" means what `du` reports for one directory on its own.
"Unique" means bytes left after hard links are counted once (one `du` call over several directories).

---

## 1. Headline numbers

| Metric | Value |
|---|---|
| Tracked files at HEAD | **4,543** blobs + 1 submodule gitlink, in 994 directories, max depth 8 |
| Tracked bytes at HEAD | **119 MiB** (raw, uncompressed) |
| Median / p95 / p99 file size | 4.2 KiB / 38 KiB / 126 KiB |
| Commits (all refs) | **5,502** (master 1,780; 213 merges; 7 root commits) |
| Refs | 350 local branches, 117 remote-tracking, 4 tags, 5 stashes |
| Reachable objects | 104,800 (49,966 blobs, 49,331 trees, 5,502 commits) |
| Reachable blob bytes | 6.99 GiB raw, **311 MiB** delta-compressed on disk |
| Distinct paths ever | 17,232 |
| `.git` on disk | **58 GB** (see 3.1: only ~0.4 GB of it is live history) |
| LFS objects in local store | 113 objects, **25.6 GiB** (median 149 MiB, max 4.3 GiB). 0 LFS files at HEAD |
| Worktrees | 50 (1 main + 49 linked) |
| Commit rate | 26 commits/day mean over 208 active days (peak 303/day); 1,592 in the last 30 days |
| Files changed per commit (last 2,000 non-merge) | median **4**, p90 21, p95 **35**, p99 352, max 4,710 |
| Workspace packages (turbo) | 114 `package.json` files |

---

## 2. Working tree at HEAD

### 2.1 Size distribution

| Bucket | Files | Bytes |
|---|---|---|
| < 1 KiB | 807 | 0.4 MiB |
| 1-10 KiB | 2,635 | 11.0 MiB |
| 10-100 KiB | 1,039 | 24.8 MiB |
| 100 KiB-1 MiB | 50 | 12.0 MiB |
| 1-10 MiB | 11 | 28.7 MiB |
| > 10 MiB | 1 | 42.3 MiB |

99% of files are under 130 KiB. 12 files (0.3%) hold 59% of the bytes.

### 2.2 Biggest files at HEAD

| Size | Path |
|---|---|
| 42.3 MiB | `models/soma/production/asc-dsl.step.int8.onnx` (was LFS, converted to a plain blob by the `unlfs` commit) |
| 7.7 MiB | `packages/ai-corpus/data/soma-train/orchestrator/orchestrator-generated.finetune.jsonl` |
| 3.7 MiB | `apps/intelligence/ai-service/.nlp-data/semantic-index.json` |
| 3.3 MiB | `apps/clients-bolton/nela/risk-score-validation-testing/fixtures/test-data.csv` |
| 3.2 MiB | `packages/ai-corpus/data/soma-train/json-render/json-render.finetune.jsonl` |
| 2.7 MiB | `packages/dsl/core/rust/js/wasm/dsl_wasm_bg.wasm` (generated, committed; changed in 47 of the last 2,000 commits) |
| 2.0 MiB | `.../risk-score-validation-testing/fixtures/sim_data_v4` |
| 1.6 / 1.2 MiB | two `.xls` fixtures in `risk-score-validation-testing/fixtures` |
| 1.1 MiB | `plugins/tracedecay/cursor-native-extension/embedded/extension.js` |
| 1.1 MiB | `bun.lock` |

### 2.3 Binary vs text

`git ls-files --eol`: 4,480 text files (LF), 41 binary (`-text`), 22 empty, 1 gitlink. Binary files are PNG/ICO/WASM/ONNX/XLS.
Line endings are LF everywhere (`core.autocrlf=input`). The `.gitattributes` file is now fully commented out (the LFS rules were removed).

### 2.4 By extension

| Ext | Files | Bytes |
|---|---|---|
| ts | 2,616 | 22.3 MiB |
| json | 426 | 11.4 MiB |
| surql | 287 | 1.7 MiB |
| md | 278 | 2.3 MiB |
| rs | 188 | 2.0 MiB |
| py | 152 | 1.6 MiB |
| tsx | 84 | 1.7 MiB |
| css | 72 | 0.5 MiB |
| toml | 58 | 0.4 MiB |
| jsonl | 14 | 15.0 MiB |
| onnx | (1) | 42.3 MiB |

### 2.5 By top-level directory (two levels)

`packages/` holds 2,250 files and `apps/` holds 1,858. The biggest by bytes are `models/soma` (42 MiB, 3 files), `packages/ai-corpus` (13 MiB, 7 files),
`apps/clients-bolton` (10 MiB), `packages/dsl` (9 MiB, 419 files), `apps/delta` (6 MiB, 473 files) and `packages/soma` (5.5 MiB, 431 files).
114 workspace packages: the median package has 15 tracked files.

### 2.6 Submodule

`packages/soma/sdk` -> `../soma.git` (1,450 files, 309 commits). Its git dir uses an **alternates** file pointing at `D:/dev/soma/.git/objects`,
but it has its **own 3.3 GiB LFS store**. Every worktree that runs `git submodule update` gets another copy of that LFS store (see 3.1).
CI checks out with `submodules: recursive` in every job.

---

## 3. History and storage

### 3.1 Where the 58 GB in `.git` goes

| Path | Size | What it is |
|---|---|---|
| `.git/worktrees/` | **27 GB** | 49 worktree admin dirs. 9 of them contain `modules/soma-sdk`, a full submodule git dir (~3.3-4.1 GiB each, mostly LFS objects) |
| `.git/lfs/objects` | **26 GB** | 113 LFS objects. All are now history-only (0 LFS pointers at HEAD; 128 pointer versions in history) |
| `.git/objects` | 4.9 GB | 4 packs, 4.63 GiB. **4.61 GB is one cruft pack** (`*.mtimes`) of unreachable objects left after `git filter-repo` and the LFS-to-Garage move. Reachable packs are ~360 MB. Plus 245 MiB of `tmp_obj_*` garbage |
| `.git/modules/` | 3.3 GB | main checkout's soma-sdk submodule git dir (LFS) |
| other | ~0.1 GB | `cursor/` 65 MB, hooks, logs, refs |

**Live history is about 0.4 GB.** About 57 GB of `.git` is LFS blobs, duplicated submodule LFS stores and unreachable cruft.

### 3.2 Largest blobs in history (top 30, raw size)

All 30 are training corpora, now moved to Garage. They delta-compress very well; many are 0-5 MiB on disk.

| Raw | On disk | Path |
|---|---|---|
| 176.3 MiB | 33.4 | `packages/ai-corpus/data/soma-train/nl-base/govuk-english.finetune.jsonl` |
| 151.2 | 8.0 | `.../nl-base/nl-controlled.finetune.jsonl` |
| 107.6 | 1.3 | `.../soma-train/govuk-english.finetune.jsonl` |
| 74.0 | 5.1 | `.../schema-authoring/schema-authoring.finetune.jsonl` |
| 72.0 | 0.8 | `.../schema-authoring/schema-authoring.finetune.jsonl` |
| 67.2 | 4.9 | `.../soma-train/govuk-english.finetune.jsonl` |
| 65.8 | 17.0 | `.../nl-base/govuk-english.jsonl` |
| 65.4 | 3.5 | `.../schema-author-big/schema-author-oneshot.finetune.jsonl` |
| 64.0 | 0.0 | `.../soma-train/govuk-english.jsonl` |
| 63.5 | 0.1 | `.../schema-authoring/schema-authoring.finetune.jsonl` |
| 63.3 | 0.1 | same path, other version |
| 62.6 | 0.0 | `.../soma-train/govuk-english.jsonl` |
| 61.3 | 0.0 | `.../schema-author-oneshot.finetune.jsonl` |
| 59.4 | 0.0 | `.../soma-train/govuk-english.jsonl` |
| 56.0 | 1.2 | `packages/soma/cloud/kaggle/_upload/soma/.cache/asc-dsl-grounded.npz` |
| 55.2 | 2.1 | `.../schema-author/schema-author-traced.finetune.jsonl` |
| 54.4 (x3) | 2.3 / 0.0 / 2.3 | `.../asc-dsl/asc-dsl-merged.finetune.jsonl` |
| 51.5 | 0.0 | `.../schema-author-traced.finetune.jsonl` |
| 50.9 | 1.6 | `.../.cache/asc-dsl-grounded.patches.npz` |
| 48.7 | 0.0 | `.../schema-author-traced.finetune.jsonl` |
| 46.0 (x2) | 1.8 | `packages/ai-corpus/data/asc-dsl.finetune.jsonl` |
| 45.7 | 0.0 | same |
| 45.4 | 1.2 | `.../schema-author-traced.finetune.jsonl` |
| 45.0 (x2) | 0.6 | `packages/ai-corpus/data/asc-dsl.finetune.jsonl` |
| 44.6 | 0.0 | `.../soma-train/schema-author-traced.finetune.jsonl` |
| 44.5 | 1.3 | `.../.cache/form-extract.patches.npz` |

Point for asc: large **append-mostly JSONL** files, each rewritten many times. Whole-file content addressing stores each version in full
(7 GiB raw against 311 MiB packed). asc needs chunked or delta storage for large blobs (for example content-defined chunking above ~1 MiB),
or it will be about 20x worse than git on this history.

### 3.3 LFS and the artefact store (existing S3 precedent)

- LFS store 25.6 GiB; the largest objects are 4.3 GiB, 1.9 GiB (x2), 1.8 GiB and 1.6 GiB. LFS endpoint is Forgejo.
- The team already moved large data out of git into **Garage (S3-compatible)**: `ops/artefacts/manifest.json` lists 578 artefacts,
  **102 GiB** total, keyed by sha256 under `bucket/path/sha256`. 8 are `required` (0.5 GiB). `bun run artefacts:pull` fetches them,
  and the `postinstall` hook runs it.
- So the pattern is proven in-house: a content-addressed blob in S3 plus a manifest pointer. asc can take this in directly (an artefact is a
  lazily fetched large blob) and remove the separate manifest tool.

### 3.4 Refs and branches

- 350 local branches by prefix: `feat/` 94, `worktree-agent-*` **79** (Claude Code agent worktree branches), `fix/` 63, `pr-*` 23, `backup/` 15, `stack/` 13, `repair/` 10, `chore/` 10.
- 82 branch names match agent/worktree/session patterns. 54 commit subjects are agent checkpoints ("Agent host session ... baseline checkpoint").
- Commits ahead of master per branch: median 0 (most branches are merged or stale), p90 394, max 954. Many long-lived divergent stacks exist.
- Authors: in practice one human (three name spellings, 5,495 of 5,502 commits) plus agents. One human runs ~20 parallel agent sessions.

---

## 4. Change patterns (last 2,000 non-merge commits, all refs)

| Metric | Value |
|---|---|
| Files per commit | mean 24.8, median **4**, p90 21, p95 **35**, p99 352, max 4,710 |
| Two-level dirs (approx. packages) touched per commit | median **1**, p90 4, p95 6 |
| Distinct paths touched | 11,473 |
| Commits/day (all refs) | mean 26.5 over 208 active days; peaks 303, 221, 203, 198 |
| Commits/day (master) | 18.2 over 98 active days |
| Commits in last 30 / 90 days | 1,592 / 3,651 |

Hottest files: `bun.lock` (151 of 2,000 commits), `.forgejo/workflows/ci.yml` (106), `e2e.yml` (89), `dsl-native.yml` (82),
`schema-designer-v2/locales/en-GB.json` (70), root `package.json` (50), `dsl_wasm_bg.wasm` (47).

Meaning: a typical changeset is small (4 files, 1 package), so Bonsai-style "changed files vs first parent" changesets are tiny and diff/affected
queries are cheap. The p99 tail (350-4,700 files) comes from mass formatting, renames and agent sweeps. asc must handle these without
per-file round trips (batch RPCs).

---

## 5. Languages and toolchains

- **TypeScript**: 2,700 files (ts+tsx); Bun 1.4 workspaces with a hoisted linker (`bunfig.toml: linker = "hoisted"`, because rspack/rslib need
  hoisted `@asc/*` links). Turborepo 2.x, 114 workspaces, `concurrency: 100`.
- **Rust**: 188 `.rs` files in separate cargo workspaces (no root workspace, but there is a stray root `Cargo.lock`):
  - `apps/analysis-pipeline` (analytics-kernels, pipeline-core, pipeline-worker, pipeline-api)
  - `packages/dsl/core/rust` (dsl-cli, -core, -cranelift, -grammar, -ir, -napi, -surrealism, -wasm-codegen, -wasm) with napi and wasm npm outputs
  - `apps/clients-bolton/nela/risk-score-wasm`, `packages/soma/soma-candle-runner`
  - submodule `soma/sdk/control` (soma-agent, soma-control, soma-proto)
  - Local `target/` dirs: dsl-surrealism 2.1 GiB, analysis-pipeline 1.3 GiB (debug).
- **Python**: 152 `.py` files (`packages/soma/models` 84, `packages/soma/core` 63). `pyproject.toml` in `packages/soma/core/python`,
  and in the submodule (`pyproject.toml` + `uv.lock`, plus 2 fixture plugins). uv cache at `D:\Jordan\cache\uv`. `packages/soma/sdk/.venv` = **2.9 GiB** (torch + CUDA DLLs).
- Other: SurrealQL (287 `.surql`), R (7), shell.
- CI (`.forgejo/workflows`, 7 files, 1,246 lines): `ubuntu-latest`, `actions/checkout` with **`submodules: recursive`** in every job
  (17 checkouts in total), `bun install --frozen-lockfile` per job, `turbo run build --filter=<x>...` with hand-written filter lists, **no turbo
  remote cache** configured (no `TURBO_*` vars), and `deploy-affected.yml` uses `fetch-depth: 0` (full history) for its affected-package diff.
  `turbo.json` sets `"env": ["*"]` on build, which puts every environment variable into the hash and destroys cross-machine cache hits.

---

## 6. Worktree disk use

### 6.1 Per-worktree measurements

Apparent size from `du -sk` per directory. `nm` = all `node_modules` dirs (pruned at the first level); `build` = `.turbo`, `dist`, `target`,
`.next`, `.venv`, `build`, `.cache`, `__pycache__` outside node_modules.

| Worktree | Total GiB | node_modules GiB (apparent) | build outputs GiB | other GiB (tracked + untracked) |
|---|---|---|---|---|
| `D:/dev/asc-e2e-verify` | 9.28 | 2.76 | 3.11 | 3.42 |
| `D:/dev/asc-i18n` | 7.20 | 2.61 | 0.27 | 4.33 |
| `.claude/worktrees/agent-a1ef4804251106b90` | 0.09 | 0.00 | 0.00 | 0.09 |
| `.claude/worktrees/agent-a2375471a58ad7d92` | 0.09 | 0.00 | 0.00 | 0.09 |
| `.claude/worktrees/agent-a2edfc5452781a2df` | 0.09 | 0.00 | 0.00 | 0.09 |
| `.claude/worktrees/agent-a345f5888746979e4` | 0.09 | 0.00 | 0.00 | 0.09 |
| `.claude/worktrees/agent-a358722374876e992` | 0.09 | 0.00 | 0.00 | 0.09 |
| `.claude/worktrees/agent-a384b35ba25a19469` | 2.70 | 2.61 | 0.00 | 0.09 |
| `.claude/worktrees/agent-a4247a22f56656ce1` | 2.76 | 2.62 | 0.05 | 0.09 |
| `.claude/worktrees/agent-a4d4740bda4a5b185` | 0.09 | 0.00 | 0.00 | 0.09 |
| `.claude/worktrees/agent-a53eb5dcc88b55e67` | 2.70 | 2.61 | 0.00 | 0.08 |
| `.claude/worktrees/agent-a54f5bf23a83c56a0` | 0.09 | 0.00 | 0.00 | 0.09 |
| `.claude/worktrees/agent-a55f13b1d4405f9fd` | 0.09 | 0.00 | 0.00 | 0.09 |
| `.claude/worktrees/agent-a5b04378a76e579ac` | 0.09 | 0.00 | 0.00 | 0.09 |
| `.claude/worktrees/agent-a81e385bcc785af58` | 0.09 | 0.00 | 0.00 | 0.09 |
| `.claude/worktrees/agent-aa4c3c12073a26527` | 0.09 | 0.00 | 0.00 | 0.09 |
| `.claude/worktrees/agent-aab33ef8e49513906` | 2.74 | 2.61 | 0.00 | 0.13 |
| `.claude/worktrees/agent-aab9706119bb5f213` | 0.13 | 0.00 | 0.00 | 0.13 |
| `.claude/worktrees/agent-acb17dce87de4f89d` | 0.09 | 0.00 | 0.00 | 0.09 |
| `.claude/worktrees/agent-ad73973712c3edede` | 0.09 | 0.00 | 0.00 | 0.09 |
| `.claude/worktrees/agent-ae3fc6757f91f233e` | 0.09 | 0.00 | 0.00 | 0.09 |
| `.claude/worktrees/agent-afaed5e22c937c36e` | 0.09 | 0.00 | 0.00 | 0.09 |
| `.claude/worktrees/feat-asc-framework-ssr` | 8.93 | 3.99 | 0.77 | 4.16 |
| `.claude/worktrees/pf-ci` | 6.74 | 2.83 | 0.40 | 3.50 |
| `.claude/worktrees/pf-final` | 8.43 | 2.85 | 1.95 | 3.62 |
| `.claude/worktrees/pf-merge` | 8.01 | 2.65 | 0.94 | 4.41 |
| `D:/dev/asc-soma` | 66.15 | 0.00 | 0.00 | 66.15 |
| `D:/Jordan/Temp/dds-pr-fixes/pr-165-linear` | 0.00 | 0.00 | 0.00 | 0.00 |
| `D:/Jordan/Temp/dds-pr-fixes/pr-166` | 0.08 | 0.08 | 0.00 | 0.00 |
| `D:/Jordan/Temp/dds-pr-fixes/pr-166-linear` | 0.03 | 0.03 | 0.00 | 0.00 |
| `D:/Jordan/Temp/dds-pr-fixes/pr-177` | 0.08 | 0.08 | 0.00 | 0.00 |
| `D:/Jordan/Temp/dds-pr-fixes/pr-177-linear` | 0.00 | 0.00 | 0.00 | 0.00 |
| `D:/Jordan/Temp/dds-pr-fixes/pr-178` | 0.06 | 0.05 | 0.00 | 0.00 |
| `D:/Jordan/Temp/dds-pr-fixes/pr-178-linear` | 0.00 | 0.00 | 0.00 | 0.00 |
| `D:/Jordan/Temp/dds-pr-fixes/pr-179` | 0.06 | 0.05 | 0.00 | 0.00 |
| `D:/Jordan/Temp/dds-pr-fixes/pr-179-linear` | 0.00 | 0.00 | 0.00 | 0.00 |
| `D:/Jordan/Temp/dds-soma-release` | 0.27 | 0.23 | 0.01 | 0.02 |
| `D:/tmp/cd-images` | 3.02 | 2.84 | 0.06 | 0.13 |
| `D:/tmp/dsl-syntax` | 0.13 | 0.00 | 0.00 | 0.13 |
| `D:/tmp/pr165` | 3.20 | 2.69 | 0.36 | 0.14 |
| `D:/tmp/pr166` | 3.36 | 2.86 | 0.36 | 0.14 |
| `D:/tmp/pr177` | 3.39 | 2.87 | 0.38 | 0.14 |
| `D:/tmp/pr178` | 3.38 | 2.87 | 0.38 | 0.14 |
| `D:/tmp/pr179` | 3.38 | 2.87 | 0.37 | 0.14 |
| `D:/tmp/pr180` | 3.73 | 2.87 | 0.71 | 0.15 |
| `D:/tmp/pr180-ci` | 0.13 | 0.00 | 0.00 | 0.13 |
| `D:/tmp/remove-repo-audit` | 0.13 | 0.00 | 0.00 | 0.13 |
| `D:/tmp/tsdocs` | 2.76 | 2.63 | 0.00 | 0.13 |
| `E:/asc-stack` | 3.43 | 3.31 | 0.01 | 0.11 |
| **All 49 linked worktrees** | **157.7** | **54.5** | **10.1** | **93.1** |

### 6.2 node_modules is mostly hard links already

`fsutil hardlink list` on `node_modules/zod/index.js` in `asc-i18n` shows **19 links**: the bun global cache `D:\Jordan\cache\bun` plus 18 worktrees.
Bun 1.4 on Windows installs with **hard links from its global cache** when the cache and project are on the same volume.
One `du` over the bun cache (7.9 GiB) plus the node_modules of `asc-i18n`, `asc-e2e-verify` and `pf-ci` gives **35-40 MiB of unique
bytes per worktree** against 2.7-3.0 GiB apparent. A `/d/tmp/pr165` install adds 36 MiB unique.

Exceptions:
- The **main checkout's** `node_modules` is **2.1 GiB unique** (link count 1; installed before the cache was populated or by a copy backend).
- `E:/asc-stack` is on another volume, so hard links are impossible and it holds a full copy.
- The real cost of hard-linked node_modules is **file count, not bytes**: ~150-180k files per worktree (MFT records ~1 KiB each, so
  ~150-180 MiB of MFT per tree), install time, Defender scanning and Explorer/`du` slowness.

### 6.3 Python venvs are copies in some worktrees

`packages/soma/sdk/.venv` (2.9 GiB): in the main checkout the torch DLLs have link count 2 (uv hard link from cache). In `asc-e2e-verify` they have
link count **1** (a real copy). Each worktree that runs `uv sync` in the submodule can add 2.9 GiB.

### 6.4 Main checkout (excluding `.git` and `.claude/worktrees`)

| Path | Size | Tracked? |
|---|---|---|
| `packages/soma/corpus/data` | **135 GiB** | No: ignored artefacts pulled from Garage |
| `packages/soma/sdk` | 6.2 GiB | submodule: benchmarks 3.3 GiB, `.venv` 2.9 GiB |
| `apps/intelligence/ai-service/.models-cache` + `.models-gec` | 10.5 GiB | No: model caches |
| `apps/clients-bolton/nela/risk-score-validation-testing` | 5.9 GiB | mostly untracked fixtures |
| `node_modules` (root) | 3.1 GiB apparent / 2.1 GiB unique | No |
| Rust `target/` (2 dirs) | 3.4 GiB | No |
| `graphify-out` | 1.3 GiB | No |
| `models/` | 660 MiB | 42 MiB tracked; rest artefacts |
| `.turbo` | 206 MiB | No |
| Tracked source | ~0.12 GiB | Yes |

`D:\dev\asc-soma` (a linked worktree) is 66 GiB: `packages/ai-corpus` 44 GiB and `packages/model-nl-base` 22 GiB of untracked
corpus/model artefacts.

### 6.5 Total and what asc saves

Linked-worktree types seen:
- **Bare agent worktrees** (14 of the 18 `agent-*` dirs under `.claude/worktrees`, plus `pr-*-linear`, `dsl-syntax`, `remove-repo-audit`): ~90-135 MiB each. That is just the
  tracked files: no install, no submodule.
- **Installed worktrees** (asc-i18n, asc-e2e-verify, pf-*, feat-asc-framework-ssr, 4 agent worktrees, `D:/tmp/pr*`, tsdocs, cd-images, E:/asc-stack):
  2.8-9.7 GiB apparent each. Of that, 2.7-4.2 GiB is hard-linked node_modules, up to 3.3 GiB is build output and **3.3 GiB is the materialised
  soma-sdk submodule** (`sdk/benchmarks` LFS) in 6 of them.
- **asc-soma**: 66 GiB of untracked corpus/model data.
- `D:/Jordan/Temp/dds-pr-fixes/pr-*`: mostly-deleted index states, 1-80 MiB.

Whole-footprint accounting for the 50 worktrees (apparent GiB, rounded):

| Component | Apparent | Real (hard links counted once) | Does an asc virtual workspace remove it? |
|---|---|---|---|
| `.git` (LFS 26 GB, per-worktree submodule git dirs 27 GB, cruft pack 4.6 GB, live packs 0.4 GB) | 54 | 54 | **Yes**: replaced by one per-machine content-addressed cache that holds only blobs that were read (est. < 2 GiB) |
| Tracked files materialised in 50 working copies (~0.1 GiB each) | 5 | 5 | **Yes**: metadata-only until read; hydrated blobs shared through the daemon cache |
| soma-sdk submodule working copies (6 x 3.3 GiB) | 20 | 20 | **Yes, if the submodule is taken into asc** (lazy large blobs) |
| node_modules (25 installs + main) | 58 | ~7 (bun cache 7.9 shared; main 2.1 unique; E: 3.3 copy; ~36 MiB per other install) | **No** |
| Build outputs (`target`, `dist`, `.turbo`, `.venv` copies) | 14 | ~12 | **No** (turbo/cargo remote cache can) |
| Untracked artefacts (corpus 135 GiB main + 66 GiB asc-soma, model caches 10.5 GiB, NELA fixtures 5.9 GiB, graphify-out) | ~220 | ~220 | **Only if committed as asc large-file pointers** (lazy) |
| **Total** | **~370** | **~320** | asc alone: **~79 GiB (~25%)**; with artefacts as asc pointers: **up to ~290 GiB (~90%)** |

Conclusions:
- The "git worktrees are eating the disk" pain is mostly not working-tree files (5 GiB in total). It is **(a) per-worktree submodule clones with
  their own LFS stores (27 GB + 20 GiB checked out), (b) history-only LFS and cruft in `.git` (31 GB), and (c) untracked corpus artefacts
  copied per checkout (200+ GiB)**. asc removes (a) and (b) by design. It removes (c) only if artefacts become first-class lazy blobs.
- node_modules looks like 58 GiB in Explorer, but bun hard links make it ~7 GiB real. Deleting worktrees frees little of it. The real cost is
  ~150-180k files per install (MFT, Defender, install time).
- Per new working copy, asc cost target: < 50 MiB (metadata, index and the files actually read), against 0.1 GiB (bare git worktree) and
  7-10 GiB (installed worktree with submodule) today.

---

## 7. Scale targets for asc

| Dimension | ASC today | asc design target (10x headroom) |
|---|---|---|
| Files per snapshot | 4.5k (+1.5k submodule) | 100k |
| Snapshot bytes | 119 MiB | 5 GiB |
| Directories per snapshot | ~1k | 20k |
| Largest single file | 42 MiB tracked; 4.3 GiB LFS; 102 GiB artefacts | multi-GiB, streamed and chunked, never held in RAM |
| Changesets | 5.5k | 1M (skip lists and generation numbers from day one) |
| Changeset size | median 4 files, p99 352, max 4.7k | 50k files in one changeset, batched |
| Commit rate | 26/day mean, 300/day peak, about 20 concurrent agents | 50 commits/min burst on one bookmark (CAS ref contention) |
| Bookmarks (branches) | 350 + 117 remote | 10k, cheap to create and delete, auto-expiring agent bookmarks |
| Concurrent working copies per machine | 50 | 200, each O(metadata) on disk |
| Large append-mostly blobs | JSONL corpora, 45-176 MiB, rewritten often | content-defined chunking so a small append costs a few chunks |

---

## 8. What asc alone will NOT fix, and complementary fixes

1. **node_modules** (bytes are already deduped by bun hard links; file count is not). Virtual workspaces do not help, because node_modules is generated
   and not versioned. Fixes: keep bun's global cache on the same volume as every worktree (move `E:/asc-stack` or give it its own cache);
   reinstall the main checkout so it links to the cache (frees ~2 GiB); try `linker = "isolated"` again when rspack supports it (one store, symlinked);
   longer term, an asc-daemon-managed `node_modules` overlay keyed by the `bun.lock` hash and shared read-only across workspaces with the same lockfile
   (a junction/symlink to a per-lockfile store), or Yarn PnP-style resolution.
2. **Python venvs** (2.9 GiB torch per copy). Use `UV_LINK_MODE=hardlink` with the uv cache on D:, or one shared venv per `uv.lock` hash
   (junction), or do not sync the SDK venv in worktrees that do not run Python.
3. **Build outputs** (`target/` 1-2 GiB each, `.turbo`, `dist`). Use a shared `CARGO_TARGET_DIR` per toolchain, or sccache; a turbo remote cache on the
   same S3 store as asc; `turbo prune`-style outputs fetched lazily from the cache instead of built per worktree.
4. **Untracked artefacts** (135 GiB corpus, 10 GiB model caches, 66 GiB in asc-soma). These are outside version control. asc can take them in as
   lazily materialised large blobs (they are already content-addressed in Garage, see 3.3), but only if they are committed as asc pointers. Otherwise
   they stay a per-checkout copy. This is the biggest single disk consumer on the machine.
5. **Existing git debris** that a one-off clean-up would reclaim today (the user's decision; nothing was done here): 4.6 GB cruft pack, 26 GB of
   history-only LFS objects (still on Forgejo), 27 GB of per-worktree submodule LFS copies, 245 MiB of tmp objects.
   About 57 GB from `.git` alone, without asc.
6. **Turbo hashing hygiene**: `"env": ["*"]` on `build` makes cache keys machine-specific. asc's hash-without-hydration only pays off once task
   inputs are deterministic.
7. **Submodule**: asc should take `soma/sdk` into the monorepo (or model it as a nested asc repo with a pinned changeset). Git submodules plus
   alternates plus per-worktree LFS are the main source of the duplication.
