# Turborepo and CI integration for asc

Status: research, 2026-10-22. Scope: how Turborepo talks to source control, how asc can make turbo hash files without hydrating a virtual checkout, how ASC's Forgejo CI works today, and the proposed asc CI flow.

Sources were read directly, not from memory:

- Turborepo **v2.10.11** (the version pinned in `D:\dev\monorepo\package.json`), sparse clone of `github.com/vercel/turborepo` at tag `v2.10.11`. File references below are relative to that repo.
- `D:\dev\monorepo`: `.forgejo/workflows/*.yml`, `.forgejo/actions/publish-images/action.yml`, `turbo.json`, `package.json`, `scripts/deploy-affected.ts`, `scripts/verify.ts`, `scripts/artefacts.ts`, `.gitattributes`, `.gitmodules`, `.dockerignore`, `docker-compose.yml`.
- `docs/research/forgejo.md` (sibling research). This doc uses the same names: `asc-checkout`, `ChangedPaths` RPC, and `git_bridge = off | lazy | eager`.

---

## 0. Summary of conclusions

1. **Turbo is not "git or nothing". It needs a git *index* and a few git plumbing commands.** In 2.10 the hot path reads `.git/index` in-process (gitoxide `gix-index`), compares stat data for each tracked file, and uses the blob SHA-1 from the index for every file whose stat matches. It reads file contents only for files whose stat does not match, for racy entries, and for untracked files. Turbo has no `git hash-object` subprocess any more: the hashing is done in-process.
2. **Manual mode (no `.git`) is unusable on a virtual checkout.** Turbo then walks every package and reads every file, which hydrates the whole tree, and `--affected` fails (`GitRequired`).
3. **Recommendation: the "git projection".** Each asc working copy gets a generated, blobless, read-mostly `.git`. It holds:
   - an `index` file. Its blob SHA-1s come from asc derived data, and its stat data matches the stat data the virtual filesystem reports;
   - git-bridge commits and trees, shared per machine through `objects/info/alternates`;
   - a promisor remote that serves single blobs lazily through the asc daemon.

   The upstream `git` binary and the upstream `turbo` binary then run unmodified, with zero hydration for clean files. It is the least hacky option because asc does not emulate git's CLI. It produces the data that git already knows how to read. It also fixes other git consumers in ASC: `scripts/check-schema-instances.ts` runs `git ls-files` in CI, and Biome has `vcs.useIgnoreFile` set.
4. **Affected detection before checkout.** A zero-checkout "plan" job calls the asc server `ChangedPaths(merge_base(base, head), head)` and maps the paths onto the turbo package graph. Inside a job, `turbo --affected` works natively against the projection: asc-checkout writes the base ref and the merge-base chain, so turbo never reaches "assume everything changed".
5. **The turbo remote cache is served by asc on the same object store.** asc implements the six `/v8/artifacts*` endpoints. They are tiered like everything else in asc: daemon on the runner or dev machine, then regional cache, then server, then S3 or local disk. The same endpoint feeds turbo's `experimentalCargoSccache` for the two Rust workspaces.
6. **ASC CI today pays for things asc does not fix, and for things it does.** The depth-1 tree is small: 4,544 files and 119 MB, plus a 1,450-file, 39 MB submodule. The large costs are these:
   - 5 cold `bun install`s per PR, with no cache;
   - no turbo cache at all, local or remote;
   - fixed `--filter` lists and per-package step lists, which are not based on what changed;
   - two workflows that duplicate each other (`ci-intelligence.yml` repeats a subset of `ci.yml`);
   - `fetch-depth: 0` in `deploy-affected.yml`;
   - hand-written `paths:` filters;
   - `--affected` silently widening to "everything" on PRs, because the base branch is not fetched.

   asc fixes checkout, history, affected detection and caching. The rest are cheap workflow changes that asc makes natural. **Estimated typical PR wall time:** from about 20–30 min of runner time across about 9 jobs to about 4–8 min. A docs-only or single-package change drops to 1–3 min. Section 4.7 has the assumptions.

---

## 1. Turborepo internals (v2.10.11)

### 1.1 How turbo chooses the SCM backend

`crates/turborepo-scm/src/lib.rs`:

```rust
pub enum SCM { Git(GitRepo), Manual }
SCM::new(path) = GitRepo::find(path).map(SCM::Git).unwrap_or(SCM::Manual)
```

- `GitRepo::find` needs **both** of these: a `git` binary found by `which::which("git")`, and a successful run of `git rev-parse --show-cdup` in the turbo root (`find_git_root`). If either fails, turbo falls back to `SCM::Manual` without any message (debug log only).
- `WorktreeInfo::detect` (`worktree.rs`) does not spawn git. It walks up to a `.git` entry. A directory means the main worktree. A file holding `gitdir: …` means a linked worktree, and turbo reads `commondir` to find the main worktree root. **Linked worktrees share the main worktree's `.turbo/cache`.**
- `build_repo_index(package_count)` builds a repository-wide index only when **package_count ≥ 16**. ASC has about 121 workspace `package.json` files, so it always takes this path.

### 1.2 File hashing: every git touchpoint

A file hash in turbo is always a **git blob SHA-1**: `sha1("blob <len>\0" + bytes)`, with CRLF→LF normalisation when `.gitattributes` asks for it. It is computed in-process (`hash_object.rs`). The tests assert equality with `git hash-object`. Package and task hashes are built on top of the per-file `(path → blob SHA-1)` maps.

**Hot path: `RepoGitIndex::new_from_gix_index` (`repo_index.rs`)**

1. Resolve the git dir. A gitfile is supported. Open `<gitdir>/index` with `gix_index::File::at(..., skip_hash = true)`. If the index is absent, turbo uses the fallback path.
2. For each non-submodule entry, run in parallel: `lstat` the working file, then compare it with the entry's stat (`trust_ctime: true, check_stat: true, use_nsec: true, use_stdev: false`). This compares mtime, ctime, size and, on Unix, inode, uid and gid.
   - Stat matches and the entry is not racy: the entry is **Clean**, and turbo uses the index OID. **The file is not read.**
   - Stat differs, or the entry is racy (`mtime >= index timestamp`): `verify_candidate` **reads and hashes the file**. If the content still matches the index OID, the entry is clean.
   - `lstat` fails: the entry is **Deleted**.
3. `has_special_entries` blocks the "provably clean" shortcut used for the dirty hash. It is set when any of these is true: sparse index, submodule, `INTENT_TO_ADD` or `SKIP_WORKTREE` flags, any tracked `.gitattributes`, or a root `.gitattributes` that loads.
4. The TREE extension (cache-tree) root is kept when it covers every entry. It is compared with `git rev-parse HEAD^{tree}` to prove there are no staged changes.
5. Untracked files: `git ls-files --others --exclude-standard -z` per package prefix (`populate_untracked_for_prefixes`), together with a working-tree walk.

**Fallback path** (no usable index, or `new_from_subprocess_and_walk`), `ls_tree.rs` and `status.rs`:

| Command (exact args) | Purpose |
|---|---|
| `git rev-parse --show-cdup` | find git root |
| `git ls-tree -r -z HEAD` (whole repo, or per package path) | committed blob OIDs |
| `git diff-index … --diff-filter=ADM --no-renames` (vs HEAD) | modified / deleted tracked files |
| `git ls-files --others --exclude-standard -z` | untracked files |
| `git status --untracked-files --no-renames -z -- <pkg>` | per-package dirty set (non-index path) |

Then `hash_discovered_objects` reads and SHA-1s every file reported as dirty or untracked.

**Task `inputs`** (`package_deps.rs`):

- No `inputs`: the package's files from the index, plus dirty files and untracked files.
- `inputs` that include `$TURBO_DEFAULT$`: turbo starts from the index set, then adds:
  - literal paths, resolved by one `stat` each;
  - true globs, resolved by `globwalk` (directory enumeration).

  Only files that the index did not already hash are read. ASC's `build`, `test`, `preview` and `dev` tasks use `["$TURBO_DEFAULT$", ".env*"]`. So `.env*` costs one directory walk per package and reads only `.env*` files.
- `inputs` without `$TURBO_DEFAULT$`: globwalk, then each candidate is looked up in the index first.

**Dirty hash** (`git.rs`, used for `x-artifact-dirty-hash` and run summaries): `git rev-parse HEAD^{tree}`, `git diff HEAD --no-ext-diff --no-color`, and `git diff --cached --no-ext-diff --no-color`. These run only when the tree cannot be proven clean.

### 1.3 Change detection: `--affected`, `--filter=[ref]`, `TURBO_SCM_BASE` and `TURBO_SCM_HEAD`

- `TURBO_SCM_BASE` and `TURBO_SCM_HEAD` are ordinary config env vars (`turborepo-config/src/env.rs`: `turbo_scm_base → scm_base`, `turbo_scm_head → scm_head`). An empty value means "unset".
- `--affected` (`turborepo-lib/src/opts.rs`, then `turborepo-scope/src/filter.rs`) becomes this selector:

  ```rust
  GitRange { from_ref: TURBO_SCM_BASE, to_ref: TURBO_SCM_HEAD,
             include_uncommitted: true, allow_unknown_objects: true, merge_base: true }
  include_dependents: true
  ```

  `--filter=...[A...B]` and `--filter=[A]` use the same machinery, with flags taken from the filter syntax.
- Base resolution (`GitRepo::resolve_base`):
  1. Use the explicit base (`TURBO_SCM_BASE` or the filter ref). It is validated and must not start with `-`.
  2. Else, if `GITHUB_ACTIONS` is set:
     - for a pull request: `GITHUB_BASE_REF`, checked with `git rev-parse --end-of-options <ref>`. With `github_actions_remote_base_ref_fallback`, turbo also tries `origin/<ref>`;
     - for a push: `before` from `GITHUB_EVENT_PATH`. On the all-zeros SHA or a force push, turbo uses `<first commit>^`.

     If none of these resolve, the result is `UnableToResolveRef`.
  3. Else `main`, else `master` (`git rev-parse <branch>`).
- Changed files (`GitRepo::changed_files`):
  - `git diff-tree -r --name-only --no-commit-id -z [--merge-base] --end-of-options <base> <head> -- <turbo-root-pathspec>` (default head `HEAD`);
  - if `include_uncommitted`, also `git ls-files --others --modified --exclude-standard -z` and `git diff --name-only --cached -z`.

  All of these run with `GIT_OPTIONAL_LOCKS=0`.
- **Failure mode:** these outcomes are converted to `InvalidRange`, which means "assume all files changed" with a `warn!`:
  - `no merge base`;
  - `bad object`, when `allow_unknown_objects` is set;
  - `UnableToResolveRef`.

  On a depth-1 `actions/checkout` of a PR, the base branch does not exist locally, so **`--affected` on ASC PRs today silently selects everything**.
- Package mapping (`turborepo-repository/src/change_mapper/mod.rs`, `turborepo-scope/src/change_detector.rs`):
  - a change to `turbo.json` or `turbo.jsonc` affects all packages;
  - a file inside a package marks that package changed;
  - files outside any package go through `GlobalDepsPackageChangeMapper`, which uses root `globalDependencies` and the root package rules;
  - **lockfile:** if the lockfile changed, turbo calls `previous_content(base, lockfile)`, which runs `git show --end-of-options <base>:<path>`. It then diffs the external dependencies per workspace. If the previous content cannot be read, the result is `LockfileChangedWithoutDetails`, which affects all packages;
  - with `include_dependents`, dependents are added through the package graph.

### 1.4 Manual mode (no `.git`, or no `git` binary)

`crates/turborepo-scm/src/manual.rs`:

- An `ignore::WalkBuilder` walks each package directory. It respects `.gitignore` and yields hidden files.
- Every file is read and hashed in git blob format. CRLF rules are applied from `.gitattributes` if one is present.
- `get_current_sha`, `changed_files` and `previous_content` all return `GitRequired`. So there is no `--affected` and no `[ref]` filter, and the run summary has no SHA.

On a lazy checkout, this is **full hydration on every turbo invocation**.

### 1.5 Local cache and worktrees

- Cache artefacts: `<cacheDir>/<hash>.tar.zst` plus `<hash>-meta.json` (`turborepo-cache/src/fs.rs`). The default directory is `.turbo/cache`. `TURBO_CACHE_DIR` overrides it.
- Linked worktrees use the main worktree's cache. For asc, set `TURBO_CACHE_DIR` to a machine-wide directory, or rely on the daemon-served remote cache (section 4.4).
- `TURBO_CACHE=local:rw,remote:r` and similar settings give per-tier read/write control. Other related variables: `TURBO_REMOTE_ONLY`, `TURBO_REMOTE_CACHE_READ_ONLY`, `TURBO_CACHE_MAX_AGE`, `TURBO_CACHE_MAX_SIZE`.

### 1.6 Remote cache HTTP API (self-hosted spec)

Spec: `apps/docs/lib/remote-cache-openapi.json`. Client: `crates/turborepo-api-client/src/lib.rs`. The base URL is `TURBO_API`, and turbo appends `/v8`. Authentication is `Authorization: Bearer $TURBO_TOKEN`. On every call, turbo adds the query parameter `teamId=` only if `TURBO_TEAMID` starts with `team_`, and adds `slug=$TURBO_TEAM` when it is set.

| Method | Path | Behaviour asc must implement |
|---|---|---|
| `GET` | `/v8/artifacts/status` | `{"status":"enabled"\|"disabled"\|"over_limit"\|"paused"}` |
| `HEAD` | `/v8/artifacts/{hash}` | 200 exists / 404 missing. 403 is treated as an auth error. |
| `GET` | `/v8/artifacts/{hash}` | Body is `application/octet-stream` (a `tar.zst` archive), plus headers `x-artifact-duration` (ms) and `x-artifact-tag` (signature) when stored. 404 means a miss. |
| `PUT` | `/v8/artifacts/{hash}` | Body is the archive. Headers: `Content-Length`, `x-artifact-duration`, optional `x-artifact-tag`, `x-artifact-sha` (git SHA of the run) and `x-artifact-dirty-hash`, plus `x-artifact-client-ci` (CI vendor). Reply 202 `{"urls":[...]}`. |
| `POST` | `/v8/artifacts` | `{"hashes":[...]}` → `{hash: {size, taskDurationMs, tag?} \| {error:{message}} \| null}` |
| `POST` | `/v8/artifacts/events` | Array of `{sessionId, source: LOCAL\|REMOTE, event: HIT\|MISS, hash, duration?}`. Return 200 and keep the events as metrics. |

Details that matter:

- **Signing.** With `remoteCache.signature: true` in `turbo.json` and `TURBO_REMOTE_CACHE_SIGNATURE_KEY` set, the tag is `HMAC-SHA256(key, prefix ‖ hash ‖ teamId ‖ body)` (`turborepo-cache/src/signature_authentication.rs`). The server stores the tag opaquely and returns it on `GET`. The client verifies it. A server that is compromised or poisoned cannot forge artefacts without the key.
- **Preflight** (`TURBO_PREFLIGHT`). With preflight, turbo sends `OPTIONS` first. The server may answer with a `Location` redirect, for example a presigned S3 URL, and can disallow the Authorization header on the redirected request. **asc can use this to make S3 serve the bytes directly:** answer the `OPTIONS` with a presigned GET or PUT URL, so artefact bytes never pass through the asc server.
- Timeouts: `TURBO_REMOTE_CACHE_TIMEOUT` and `TURBO_REMOTE_CACHE_UPLOAD_TIMEOUT`.
- **`futureFlags.experimentalCargoSccache`** (`turborepo-sccache-proxy`): turbo starts a loopback WebDAV-style proxy and stores per-rustc compilation units as remote-cache artefacts. It runs in CI only. It needs `experimentalCargoWorkspaces` and a configured remote cache. Rust compile caching therefore comes for free on the same endpoint.
- **`futureFlags.experimentalCargoWorkspaces` and `experimentalPythonWorkspaces`** (uv): these make Cargo crates (discovered by `cargo metadata` from a **root** `Cargo.toml`) and uv workspace members into turbo packages, so they take part in `--affected`. ASC does not have the root `Cargo.toml` or root `pyproject.toml` that these flags need. See section 3.4.

---

## 2. Hashing without hydrating a virtual checkout

### 2.1 What turbo needs, reduced to an interface

| Need | Turbo source | Cost if served by real data |
|---|---|---|
| A `git` binary that answers `rev-parse --show-cdup`, `rev-parse HEAD`, `rev-parse HEAD^{tree}`, `rev-parse <ref>` | lib.rs, git.rs | refs and commits |
| `<gitdir>/index`: path, mode, blob SHA-1, stat, plus TREE extension | repo_index.rs | per-file metadata only |
| `lstat` per tracked file that agrees with the index | repo_index.rs | the virtual filesystem answers from metadata |
| `ls-files --others [--modified] --exclude-standard` | ls_tree.rs, git.rs | directory enumeration, no file reads |
| `diff-tree [--merge-base] base head` | git.rs | commits and trees for base, head and the merge base |
| `show base:<lockfile>` | git.rs | **one** blob, only if the lockfile changed |
| `diff HEAD` (dirty hash) | git.rs | blobs of locally modified files only |

Nothing in this table needs the bytes of an unmodified file. **The only required content is the lockfile at base, when the lockfile changed.**

### 2.2 Options

| # | Option | How | Pros | Cons | Verdict |
|---|---|---|---|---|---|
| A | Manual mode | Expose no `.git` | Nothing to build | Reads every file (full hydration), no `--affected`, no SHA in the summary | Rejected |
| B | `git` shim | An `asc-git` binary first on `PATH` that emulates the ~12 git invocations in 1.2/1.3 from asc metadata, plus a synthetic `.git/index` | No git objects at all | Emulates git's CLI output formats (`ls-tree -z`, porcelain `-z`, `diff-tree -z`), which is fragile across turbo upgrades. Turbo moved from `ls-tree` to reading `gix-index` in 2.x. It breaks every other git user (`check-schema-instances.ts` runs `git ls-files`, Biome VCS, IDEs, `pr-pack.ts`). Shadowing `git` on `PATH` on dev machines is dangerous. | Rejected as primary. Possible as a test double only. |
| C | **git projection** | asc writes a real, blobless `.git` for each working copy from git-bridge derived data. The real git binary runs unmodified. | No emulation. Every git reader works (turbo, Biome, IDE, scripts). Turbo's fastest path (index plus stat) runs with zero hydration. Lazy blob fetch uses git's own partial-clone feature. | Needs `git_bridge` derived data (git commit and tree IDs) for the repository, which is already planned in `forgejo.md`. Needs a small local "promisor" server for blobs. The index must stay consistent with the virtual filesystem. | **Recommended** |
| D | Turbo fork with `SCM::Asc` | Add a third SCM variant that calls the asc daemon socket (`ChangedPaths`, `GitBlobIds`) | Fastest possible. Server-side diff with no local commits. | Forks a project that releases weekly. Maintenance cost. Users must install a forked turbo. | Rejected. Revisit as an upstream proposal (2.5). |
| E | Turbo daemon reads hashes from asc | Turbo's daemon (`turborepo-daemon`) uses a file watcher plus SCM. It has no plugin point for hashes. | – | Needs D anyway | Rejected |

### 2.3 Design of the git projection (option C)

Layout for a working copy `W`. The `.git` entry is a **gitfile**, so git metadata never lives inside the virtualisation root:

```
W/.git                       -> "gitdir: ~/.asc/wc/<wc-id>/git"     (file, written once)
~/.asc/wc/<wc-id>/git/
  HEAD                       -> "ref: refs/heads/<branch>" (or detached bridge SHA)
  config                     -> core.bare=false, core.worktree=W, core.fsmonitor=<asc hook>,
                                extensions.partialclone=asc,
                                remote.asc.url=asc::<repo>, remote.asc.promisor=true,
                                remote.asc.partialclonefilter=blob:none
  index                      -> generated by asc (v2/v4, with TREE extension, no sparse/skip-worktree)
  refs/heads/<branch>, refs/remotes/origin/<base>   -> bridge commit SHA-1s
  objects/info/alternates    -> ~/.asc/gitbridge/<repo>/objects   (machine-wide, shared by all WCs)
  shallow                    -> optional, see below
~/.asc/gitbridge/<repo>/objects/   commits + trees only (blobless), plus blobs fetched on demand
```

**Derived data the asc server must produce.** These are Scion-style derived data, computed from parents. They are eager on push for repositories with turbo, and lazy otherwise.

1. `GitBlobId(file content id) -> sha1`. This is cheap: compute it when the blob is ingested, since the bytes are already in hand. Store it next to the asc content hash in the file entry, so it is part of the **tree metadata** a client fetches during negotiation. It must be computed over the **canonical stored bytes**, which matches git's view when the repository has no eol or filter conversions. ASC's `.gitattributes` is 100% comments, and the files are stored LF.
2. `GitTreeId(asc tree)` and `GitCommitId(changeset)`: the git bridge (`forgejo.md`, `git_bridge`). They must be deterministic: fixed author and committer mapping, message, and parents mapped through the bridge. Each one is computed from its children and parents, recursively, Scion-style.
3. `GitBridgePack(range)`: a server RPC that returns commits and trees (no blobs) for a commit range as a plain pack. Delta compression is optional. It is cached on the regional and daemon tiers by immutable ID.

**What `asc clone`, `asc switch` and `asc-checkout` do:**

1. Mount the virtual working copy (ProjFS, FUSE or macOS fallback) at changeset `C`.
2. Make sure the bridge objects for `C` exist in the shared `~/.asc/gitbridge`. Fetch only what is missing: commits and trees are tiny compared with blobs, and they are shared by all 50 worktrees.
3. Write `index` from asc tree metadata. Each entry gets: path, mode, `GitBlobId`, size, and **mtime/ctime/ino equal to exactly what the virtual filesystem will report** for the placeholder. Set the index timestamp to `max(mtime) + 2 s` so that no entry is racy. Write the TREE extension with `GitTreeId`s, so `cache_tree_root == HEAD^{tree}`. Do not use sparse or skip-worktree flags: the virtual filesystem shows every file, so turbo's "special entries" rule is not triggered.
4. Write `HEAD`, the branch ref and the base refs. For CI: `refs/remotes/origin/<base>` and `refs/heads/<base>` at the bridge commit of the PR base.

**Keeping the index honest.** Developers will edit files, run `git add`, and so on. Rules:

- The virtual filesystem must report stable stat data for unmodified placeholders: same mtime and size, and on FUSE a stable inode. ProjFS placeholders keep the `PRJ_FILE_BASIC_INFO` that the provider wrote at creation, and hydrating a placeholder does not change its timestamps. A file the user writes gets new stat data, so turbo sees it as stat-dirty and hashes it. That is correct, and the file is already local.
- `git` writes to the index are allowed (`git add` and `git stash` keep working). `asc status`, `asc add` and `asc commit` read the same index. **asc treats the git index as its staging area**, so there is one staging area, not two. asc regenerates the index only on `asc switch`, `asc checkout` and `asc reset`.
- `core.fsmonitor` points at an asc hook that returns the virtual filesystem's dirty set. `git status` then never stats or walks unmodified directories. Turbo does not use fsmonitor itself, but other git users benefit.
- `git commit` and `git push` in an asc working copy are refused by `core.hooksPath` hooks (`pre-commit`, `pre-push`) with "use `asc commit` / `asc push`". The projection is for reading. asc owns history.

**Blobs on demand (promisor).** Git's partial clone lazily fetches missing objects from the promisor remote. That runs `git fetch` with `--filter`, using protocol v2. asc ships `git-remote-asc`, a remote helper with the `stateless-connect` capability that speaks upload-pack v2 `fetch` for explicit object IDs only (no negotiation). It answers from the daemon cache, then the regional cache, then the server, using `GitBlobId → asc content id` lookups. The pack it returns holds whole objects with no deltas, which is trivial to write. This serves `git show base:bun.lock`, `git diff HEAD` on modified files, `git blame`, and IDE "compare with HEAD".

**History depth.** `diff-tree --merge-base` needs the commit graph from `head` back to the merge base. Two options:

- Default for developers: the full first-parent chain of commits and trees in the shared alternates store. ASC has 1,780 commits, and trees are small and delta-friendly. This is paid **once per machine**, not once per worktree.
- CI: asc-checkout asks the server for `merge_base(base, head)`, writes `shallow` = {merge-base}, and fetches the bridge commits and trees for `merge-base..head` only. Turbo's `--merge-base` diff then resolves inside the shallow boundary.

**Cost per extra worktree with the projection:** one index file (about 0.5 MB for 6k entries), refs, and virtual-filesystem metadata. The working copy hydrates only what is read. Compare today: about 91 MB for an agent worktree, plus node_modules for any worktree that installed (the main checkout's is 3.1 GB). Node_modules is the subject of 4.5.

### 2.4 Contract tests

Pin a turbo version in the asc repository's CI and run turbo against a projected working copy, with a hydration counter on the virtual-filesystem provider:

1. `turbo run build --dry=json` on a clean working copy: **0 file hydrations** except `package.json` files, `turbo.json`, `bun.lock` and root config files. Turbo parses those for the package graph. The task hashes must equal the hashes from a real git clone of the same commit. This is the critical golden test.
2. Modify one file: only that file's task hashes change, and hydration count stays the same plus 1.
3. `TURBO_SCM_BASE=<base> turbo ls --affected --output=json`: the result must equal the result from a full git clone. The only blob fetched is `bun.lock` at base, and only when it changed.
4. `git status`, `git ls-files`, `git diff HEAD` and `git log -5` all work, with no hydration for clean files.
5. Racy test: check out, then run immediately within 1 s. There must be no `verify_candidate` reads. Assert this through turbo's debug logs: `verified entries = 0`.

Run the matrix on Windows (ProjFS), Linux (FUSE) and macOS for every turbo minor release. **Turbo's git touchpoints change between minors**: 2.x replaced `ls-tree` with `gix-index`. The test suite is the guard.

### 2.5 Upstream follow-up (optional, low cost)

Propose two small changes to turbo, each one useful beyond asc:

- `TURBO_SCM_CHANGED_FILES=<file>`: a NUL-separated list that replaces the `diff-tree` call. CI systems with API diffs (asc, GitHub, GitLab) could then skip history entirely. Turbo still maps files to packages and reads the previous lockfile through a path `TURBO_SCM_BASE_LOCKFILE=<file>`.
- Treat a tracked, comment-only `.gitattributes` as "no attributes". ASC can simply delete its file today, which is 100% comments. That unblocks the dirty-hash shortcut.

---

## 3. ASC CI today

### 3.1 Inventory

| Workflow | Trigger | Jobs (checkouts) | Checkout | Install | Turbo | Caches |
|---|---|---|---|---|---|---|
| `ci.yml` | PR `**`, push master | 5: typecheck-and-test, generated-artefacts, lint-conventions, rust, dsl-rust | `actions/checkout@v4.4.0`, `submodules: recursive`, depth 1 | `bun install --frozen-lockfile` ×2 (cold) | `bunx turbo run build` with 10 hand-written `--filter=X...` | `actions/cache` for cargo registry, git and `target/` (rust and dsl-rust only) |
| `ci-intelligence.yml` | PR `**`, push master | 2: typecheck-and-test, rust | same | `bun install` (no `--frozen-lockfile`) | `turbo run build --filter=insight-service... --filter=ingestion-service...` | **none**: rust job has no cargo cache |
| `e2e.yml` | PR `**`, push master | e2e (+ publish-images after e2e on push, e2e-headed on dispatch) | same | `bun install` + `cd e2e && bun install` + Playwright browsers | `bun run build` = `turbo run build` for **everything** | upload-artifact for reports |
| `docker-intelligence-pr.yml` | PR, path filter (hand list) | 1 | same | `bun install` | `turbo run build --filter` for 3 MFEs | buildx (no explicit cache-from) |
| `docker-intelligence.yml` | push master, path filter | 1 | same | `bun install` | same | same |
| `dsl-native.yml` | PR and push, path `packages/dsl/core/rust/**` | 8-target matrix, plus stage and publish | same | `bun install --frozen-lockfile` | – | – |
| `deploy-affected.yml` | push master | 1 | **`fetch-depth: 0`** + submodules | `bun install --frozen-lockfile` | `turbo ls --affected` with `TURBO_SCM_BASE=before`, `TURBO_SCM_HEAD=sha` | none |

`.forgejo/actions/publish-images` runs `docker compose build` for 8 services. Every Dockerfile uses context `.`, guarded by a long `.dockerignore` because "admin-ui context was 8.32 GB, 231 s to transfer". There are no turbo remote-cache variables (`TURBO_API`, `TURBO_TOKEN`) anywhere. No `.turbo/cache` and no `~/.bun/install/cache` is cached.

### 3.2 Where time and disk go (per PR that touches a TS package)

Measured repository facts:

| Fact | Value |
|---|---|
| HEAD tree | 4,544 files, 119 MB (largest: a 42 MB `.onnx` committed outside LFS) |
| Submodule `packages/soma/sdk` | 1,450 files, 39 MB |
| Packs | 4.63 GiB across 128k objects |
| Local LFS store | 26 GB |
| Local `.git` | 58 GB |
| History | 1,780 commits on HEAD, 467 branch refs |
| Lockfile | `bun.lock` 1.1 MB |
| Main `node_modules` | 3.1 GB |
| `.turbo` | 206 MB |

These are estimates, because runner logs were not available to this research. Treat them as orders of magnitude.

1. **Checkouts: about 9 per PR** (ci 5, ci-intelligence 2, e2e 1, docker-pr 0–1). Depth-1 checkout of about 160 MB plus the submodule clone takes roughly 15–45 s each. The server also generates a fresh pack for each one, which costs CPU on Forgejo against a 4.6 GiB repository. That adds up to about 3–6 runner-minutes per PR. `deploy-affected` with `fetch-depth: 0` pulls the full history, multiple GB with all branches, on every push to master: about 2–5 min.
2. **`bun install`: 5 cold installs per PR** (ci ×2, ci-intelligence, e2e ×2 with `e2e/`, docker-pr). The workflow's own comment says "a fresh monorepo `bun install` alone is minutes". Estimate 1.5–3 min each, so about 8–15 runner-minutes. **This is the single largest avoidable cost, and it is not a source-control problem.**
3. **Turbo builds, always cold:** ci (10 filter roots), ci-intelligence (2), e2e (`turbo run build` for everything), docker-pr (3 MFEs). The same packages are built 3–4 times per PR on different runners, with no cache, regardless of what changed. Estimate 8–20 runner-minutes.
4. **Typecheck and test steps listed by hand**, not through turbo, so they are not cached and not filtered by what changed. `ci.yml` alone runs 13 typecheck targets and 12 test targets on every PR. `scripts/verify.ts` documents why: turbo's `check-types` skips packages that define only `typecheck`. That is a naming problem, fixable by adding a `check-types` alias script.
5. **Duplicated work:** `ci-intelligence.yml` repeats a subset of `ci.yml`: insight and ingestion typecheck, the insight unit tests, the migration lint, and `cargo test` for analysis-pipeline. Its rust job has **no cargo cache**. The analysis-pipeline `cargo test` therefore runs twice per PR, once cold.
6. **Rust:** `rust` and `dsl-rust` run on every PR (25 and 30 min timeouts) even when no Rust changed. `dsl-native` is path-filtered correctly.
7. **Affected detection:**
   - `deploy-affected` works, but only because it fetches the whole history;
   - the PR workflows do not use `--affected` at all;
   - with depth 1, `--affected` would degrade to "everything" (1.3);
   - the path filters in the docker workflows restate the dependency graph by hand. That is the exact problem `deploy-affected.ts` was written to avoid.
8. **Docker:** whole-repository context, with prebuilt MFE dists copied in, no `turbo prune --docker`, and no explicit buildx cache. This is outside asc, but the asc plan job can decide which images to build.

### 3.3 What is good and should be kept

- `deploy-affected.ts` already treats turbo as the dependency-graph oracle and has a typed target map. asc should feed it, not replace it.
- `scripts/artefacts.ts` already moved model weights from LFS to S3 (Garage) behind a sha256 manifest, and skips them under `CI`. That is the same content-addressed shape as asc blobs. asc can take this over later as large-file storage, with the same Garage bucket and lazy fetch.
- The pinned action SHAs, the `concurrency` groups and the DB-free fast gate are sound.

### 3.4 Rust and Python in the turbo graph

- `packages/dsl/core/rust` is a bun workspace member with `package.json` scripts. `apps/analysis-pipeline/pipeline-{api,worker}` have `package.json` files. So turbo already sees these packages. CI just bypasses turbo for cargo.
- Turbo 2.10's `experimentalCargoWorkspaces` needs a **root** `Cargo.toml` with `[workspace.metadata] name`. ASC has two unrelated cargo workspaces and no root. Options:
  1. Keep the `package.json` wrappers and add turbo tasks such as `cargo-test` with `inputs: ["**/*.rs", "**/Cargo.toml", "Cargo.lock"]`. This is simplest, works today, and works with `--affected` and the remote cache.
  2. Later, a root virtual workspace, if the two can share a lockfile. That unlocks `experimentalCargoSccache`.
- Python: only `packages/soma/sdk/pyproject.toml`, inside the submodule. `experimentalPythonWorkspaces` would need a root uv workspace. Low value now.

---

## 4. Proposed asc CI flow

### 4.1 Topology on the runners

```
            ┌──────────────── runner host (persistent) ───────────────┐
 Forgejo ──►│ forgejo-runner ── job container (ephemeral)              │
 (asc-aware) │                      │  /work/W  (bind mount, rshared)   │
            │ asc daemon ◄──────────┘  ~/.asc cache (bind mount, ro)     │
            │  - FUSE mounts W per job (virtual WC)                    │
            │  - shared CAS cache (blobs, trees, bridge objects)       │
            │  - local turbo remote-cache tier  :/v8 on 127.0.0.1      │
            │  - bun install cache + node_modules store                │
            └────────────────────────────┬─────────────────────────────┘
                                         │ same RPCs
                              regional cache (optional) ──► asc server ──► S3 (Garage) / local disk
```

- The daemon runs **on the host**, not inside the job container. FUSE then needs no `--privileged` or `/dev/fuse` in the job. The job sees a normal directory through a bind mount with `rshared` propagation. This needs `container.valid_volumes` and `options` in the forgejo-runner config. The runner config is not in the monorepo repository, so this must be confirmed with whoever operates the runners.
- On a runner with no daemon (a cloud runner or a fork), asc-checkout falls back to an in-container daemon with FUSE if `/dev/fuse` is available. Otherwise it does a **sparse materialised checkout**: write only the paths the plan job says are needed, from the CAS, without a virtual filesystem. The same action handles both cases.

### 4.2 `asc-checkout` (replacement for `actions/checkout`)

```yaml
- uses: asc/asc-checkout@v1
  with:
    changeset: ${{ github.sha }}         # asc changeset ID (Forgejo context; mapped bridge SHA accepted)
    base: ${{ github.base_ref || github.event.before }}   # optional; enables --affected
    mode: virtual                         # virtual | sparse | full
    paths: ${{ needs.plan.outputs.paths }}  # optional prefetch / sparse set
    git-projection: true                  # write the blobless .git (section 2.3)
    hydrate: |                            # optional eager prefetch, batched in one RPC
      **/package.json
      bun.lock
      turbo.json
```

The action does the following, in order:

1. Authenticate with the Actions task token. The asc server checks it with Forgejo's internal authz endpoint (see `forgejo.md` Seam C).
2. `asc clone --virtual --at <changeset> <dir>`. Cost: the root tree plus the tree metadata the job touches. **No history, no submodule clone** (the submodule is a gitlink entry resolved by asc as a nested repository, or vendored), and **no LFS smudge**.
3. If `base` is set, call `MergeBase(base, head)` on the server. Write the git projection refs (`origin/<base>`, `shallow`) and fetch the bridge commits and trees for `merge-base..head`. Export `TURBO_SCM_BASE=<merge-base bridge sha>` and `TURBO_SCM_HEAD=HEAD`. Turbo's own `--affected` is then exact, and needs neither `GITHUB_BASE_REF` heuristics nor a fetch. Forgejo's act-based runner sets `GITHUB_*` variables. Setting `TURBO_SCM_BASE` explicitly makes the behaviour not depend on that.
4. Prefetch the `hydrate` globs in one batched `GetBlobs` RPC. Turbo and bun read every `package.json` and `bun.lock` anyway, and one round trip beats a few hundred page-fault fetches.
5. Export `TURBO_API=http://127.0.0.1:<port>`, `TURBO_TOKEN=<task token>`, `TURBO_TEAM=<owner>`, `TURBO_CACHE=local:rw,remote:rw` (or `remote:r` for untrusted PRs, see 4.4), and `TURBO_REMOTE_CACHE_SIGNATURE_KEY` from a secret.

### 4.3 Affected computation: two levels

**Level 1. The plan job, with zero checkout** (seconds). It runs once per workflow run and decides which jobs exist:

```yaml
plan:
  runs-on: ubuntu-latest
  outputs:
    packages: ${{ steps.a.outputs.packages }}   # JSON list of affected turbo packages
    rust: ${{ steps.a.outputs.rust }}           # true if any cargo package affected
    images: ${{ steps.a.outputs.images }}       # docker services to build
    e2e: ${{ steps.a.outputs.e2e }}
  steps:
    - id: a
      run: asc affected --base "$BASE" --head "$HEAD" --format=gha
```

`asc affected` does this:

1. Calls `ChangedPaths(merge_base(base, head), head)` on the asc server. This is a server-side diff of two tree IDs: the server walks only differing subtrees by hash, so the cost is proportional to the size of the change, not the size of the repository.
2. Loads the **package graph** as derived data. The server computes it per changeset and caches it by the root tree ID of the inputs (`package.json` files, workspace globs, `turbo.json`, `bun.lock`). Two ways to compute it:
   - (a) The server runs `turbo ls --output=json` in a sandbox over a virtual working copy. Only manifests hydrate. This is exact and reuses turbo's own logic.
   - (b) A small Rust reimplementation: workspace globs plus `dependencies`, `devDependencies` and `peerDependencies` with `workspace:` protocol. It is faster, but it is a second implementation that can drift.

   **Choose (a).** It is computed once per changeset and is cheap.
3. Maps paths to packages, adds dependents, and applies global rules: `turbo.json` means everything, and root files go through turbo's root-package rules.
4. If `bun.lock` changed, runs turbo's lockfile diff through (a) with `TURBO_SCM_BASE`. It does not reimplement the bun lockfile parser.
5. Maps packages to job switches: `rust` (analysis-pipeline, dsl-rust), `images` (`ops/coolify/deploy-targets.json` and the docker services), and `e2e`.

Jobs then use `if: needs.plan.outputs.rust == 'true'`. This replaces every hand-written `paths:` list. Workflow-level `paths:` filters in Forgejo also go through `ChangedPaths` once the asc backend lands (`forgejo.md` 3.3).

**Level 2. Inside a job.** Run `turbo run <tasks> --affected`. Turbo computes the exact task set and the hashes from the projection. The plan job only needs to be conservative, never exact. Turbo inside the job is the source of truth.

`deploy-affected.yml` becomes: no `fetch-depth: 0`, `asc-checkout` with `base: ${{ github.event.before }}`, and the rest of `deploy-affected.ts` unchanged. It already reads `TURBO_SCM_BASE` and `TURBO_SCM_HEAD`. Better still, run `asc affected` in the plan job with no checkout and no install at all. Deploying only needs package names plus `deploy-targets.json`, which is one blob read.

### 4.4 Turbo remote cache served by asc

**Server side** (`asc-server`, crate `asc-turbo-cache`). It implements the six endpoints in 1.6 on the asc storage layer, so S3, MinIO, R2, Garage, Ceph and local disk all work, and local-only mode works too.

```
objects:   turbo/<repo-id>/<scope>/<hash>        (tar.zst bytes, immutable)
metadata:  turbo/<repo-id>/<scope>/<hash>.meta   {size, durationMs, tag, sha(asc changeset), createdBy, createdAt}
```

- `<scope>` is the trust domain: `trusted` (protected branches, written by master and release jobs) or `pr-<n>` (written by PR jobs).
  - Reads try `pr-<n>` and then `trusted`.
  - Writes go only to the job's own scope.
  - A PR therefore cannot poison master's cache. That holds even without signature keys. Signatures (`remoteCache.signature: true`) add a second, end-to-end layer.
- Authentication is the same bearer tokens as the rest of asc: Forgejo PATs, OAuth, Actions task tokens. `teamId`/`slug` map to the Forgejo owner. Authorisation is "can read repository" for GET and HEAD, and "can write to the scope" for PUT.
- **Preflight redirects**, optional. Answer `OPTIONS` with a presigned S3 URL, so large artefacts go straight between the runner and the bucket.
- **Retention:** LRU by last access, using the metadata. Default 14 days for `pr-*` and 60 days for `trusted`. A size cap per repository.
- `x-artifact-sha` is stored as the asc changeset (via the bridge mapping). So `asc log` and the Forgejo UI can show "built by changeset X".

**Tiering, the Scion shape.** The asc **daemon** exposes the same `/v8` API on `127.0.0.1`:

- **reads:** daemon cache, then regional cache, then server, then the object store. On a hit the daemon keeps a copy, so every later job on that runner host is served from local disk;
- **writes:** written through to the server asynchronously. Turbo gets its 202 as soon as the bytes are in the local CAS, so upload time leaves the critical path. This is safe because artefacts are immutable and keyed by hash.

Developers use the same daemon. One `.turbo` cache per machine, shared across all worktrees, also replaces the per-worktree `.turbo` (206 MB on the main checkout).

**Rust:** with a cargo turbo package (option 3.4.1), `cargo test` outputs are cached by turbo task hash. With a root workspace later, `experimentalCargoSccache` stores compilation units through the same endpoint, which replaces the `actions/cache` steps for `target/`.

### 4.5 Installs (node_modules), the biggest single CI cost

asc cannot make `bun install` disappear, but the daemon host can make it warm. Use one of these two:

1. **Simple, now.** Bind-mount a host-persistent `~/.bun/install/cache` into job containers on the same filesystem as the working copy's scratch area. Bun hardlinks or clones from it, and `--frozen-lockfile` with a warm cache takes seconds. No asc needed. Do this first.
2. **asc-native.** Treat `node_modules` as a **build output keyed by `bun.lock` plus all workspace `package.json`s plus `patches/**`**. The daemon stores a content-addressed snapshot (a tree of blob IDs in the same CAS). On a match, asc mounts it read-only as a **redirection** inside the working copy, the EdenFS "redirect" idea. The redirected `node_modules` is a separate overlay mount and not part of the virtualised source tree. Install time becomes mount time, about 1 s. On dev machines this also removes the 3.1 GB per-worktree duplication: identical lockfiles share one snapshot.

In every working copy, `node_modules/`, `dist/`, `.turbo/` and `target/` must be **redirections to plain local disk**. They must not be virtualised source, because writing gigabytes through ProjFS or FUSE is slower and pollutes the asc dirty set. The redirection list belongs in a checked-in `.asc/redirections` file, defaulting to the `.gitignore` build-output globs.

### 4.6 Rewritten workflow (sketch)

```yaml
name: CI
on: { pull_request: { branches: ['**'] }, push: { branches: [master] } }
concurrency: { group: ci-${{ github.ref }}, cancel-in-progress: true }

jobs:
  plan:                                   # no checkout, ~5 s
    runs-on: ubuntu-latest
    outputs: { any: ..., rust: ..., images: ..., e2e: ... }
    steps:
      - uses: asc/asc-affected@v1        # ChangedPaths + package graph (4.3)
        id: a
        with: { base: '${{ github.base_ref || github.event.before }}' }

  verify:                                 # replaces ci.yml typecheck-and-test + ci-intelligence
    needs: plan
    if: needs.plan.outputs.any == 'true'
    runs-on: ubuntu-latest
    steps:
      - uses: asc/asc-checkout@v1
        with: { base: '${{ github.base_ref || github.event.before }}', mode: virtual }
      - uses: oven-sh/setup-bun@<sha>
        with: { bun-version: 1.2.22 }
      - run: bun install --frozen-lockfile          # warm host cache (4.5)
      - run: bunx turbo run build check-types test lint --affected
      - run: bun run check && bun run check:layout && bun run check:userdb   # cheap repo-wide gates

  rust:
    needs: plan
    if: needs.plan.outputs.rust == 'true'
    ...  # asc-checkout mode: virtual; turbo run cargo-test --affected (remote-cached)

  images:
    needs: [plan, verify]
    if: needs.plan.outputs.images != '[]'
    ...  # build only the listed services; turbo restores MFE dists from cache

  e2e:
    needs: [plan, images]
    if: needs.plan.outputs.e2e == 'true'
    ...
```

Prerequisite clean-ups in monorepo. All of them are cheap and independent of asc:

- Give every package a `check-types` script (alias `typecheck`), so `turbo run check-types` covers everything and `scripts/verify.ts`'s workaround goes away.
- Merge `ci-intelligence.yml` into `ci.yml`. Everything it does is a subset of `ci.yml`.
- Add `cargo-test` and `cargo-check` turbo tasks to the Rust `package.json` wrappers.
- Delete the comment-only `.gitattributes`, which unblocks turbo's dirty-hash shortcut (1.2).
- Turn on remote caching as soon as **any** `/v8` server exists. The asc implementation can come later. Any spec-compliant server works first, and asc replaces it without workflow changes.

### 4.7 Estimated wins

These assume: Linux runners, a persistent host with the asc daemon and warm caches, and a typical PR touching 1–3 TS packages. Today's figures are the estimates from 3.2, because no runner logs were inspected. Measure before and after with turbo `--summarize` and the Forgejo job timings.

| Item | Today (per PR) | With asc flow | Main driver |
|---|---|---|---|
| Checkouts | ~9 × 15–45 s, plus server pack generation | 3–5 × ~1–2 s mount; hydrate about 5–20 MB actually read | virtual working copy, shared host CAS, no history |
| `deploy-affected` (push) | full history fetch, 2–5 min | ~5 s, plan job only | server `ChangedPaths`, no checkout |
| Installs | 5 cold, ~8–15 runner-min | 1–2 warm, ~10–30 s each | host bun cache, later node_modules snapshot |
| Builds | 3–4 cold builds of overlapping graphs, ~8–20 runner-min | affected only, mostly cache hits, ~1–3 min | `--affected` plus asc remote cache |
| Typecheck and test | 25 fixed steps every PR | affected packages only, cached | turbo task graph |
| Rust | 2–3 cargo jobs every PR (one without cache) | skipped unless Rust affected; cached when run | plan job plus remote cache or sccache |
| Turbo hashing on a virtual working copy | n/a | same as a full checkout, with 0 content reads for clean files | git projection (2.3) |
| **Typical PR runner time** | **~25–45 runner-min, ~15–25 min wall** | **~4–8 runner-min, ~4–8 min wall** | |
| Docs-only or config-only PR | same as above | plan job + checks, ~1–3 min | plan job |

About two thirds of the win comes from caching and affected selection, which asc makes exact and cheap. One third comes from asc-specific mechanics: no history, virtual checkout, the tiered daemon cache, and the projection that lets turbo run on a lazy tree. The developer-side disk win in ASC is larger than the CI win. Worktrees cost about an index file plus metadata instead of a full tree, `.git` history is shared, LFS is gone, and `node_modules` snapshots are shared. That is the subject of the other research notes.

### 4.8 Rollout order

1. Now, no asc needed: host bun cache, merge the duplicate workflow, add the `check-types` alias, add the cargo turbo tasks, and run a stand-in `/v8` server on Garage so turbo caching exists.
2. asc server with `git_bridge = eager` for asc/dds. Ship `asc affected` (plan job). Remove the `paths:` lists. `deploy-affected` drops `fetch-depth: 0`.
3. `asc-checkout` in `sparse` mode, which needs no FUSE on the runners. Turn on the asc `/v8` cache and point `TURBO_API` at it.
4. The asc daemon on runner hosts. `asc-checkout` switches to `virtual` mode with the git projection, and the daemon cache tier comes in.
5. node_modules snapshots as redirections. Optionally a root Cargo workspace and `experimentalCargoSccache`.

### 4.9 Risks and open questions

- **Turbo upgrade drift.** The projection contract (2.4) must run for every turbo minor release. Turbo's SCM layer changes often: 2.x added `gix-index` reading, racy verification and a dirty-hash shortcut.
- **Stat fidelity on ProjFS.** Windows `gix` stat uses creation time as ctime. The ProjFS provider must keep `CreationTime`, `ChangeTime` and `LastWriteTime` constant for each placeholder across hydration. If any provider operation touches them, every file becomes "stat-dirty" and turbo hashes it, which means full hydration. That can silently destroy the benefit, so the hydration-counter test is mandatory.
- **Index ownership.** Users running `git add` or `git reset` change the index. asc must read it as the staging area, not overwrite it, except on switch, checkout and reset.
- **Forgejo runner config** (bind mounts with `rshared`, host daemon) is not in the repository. Needs confirmation from whoever runs the runners. The sparse mode in 4.2 is the fallback.
- **Fork PR security:** PR scopes in the remote cache, read-only access to `trusted`, and signature keys only on trusted jobs.
- **Submodule** `packages/soma/sdk` (`url = ../soma.git`): it must also be an asc repository, or be imported as a subtree, before asc-checkout can drop `submodules: recursive`.
