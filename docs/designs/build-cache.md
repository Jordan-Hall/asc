# asc build cache: one content-addressed cache for every toolchain

Status: design, 2026-11-04. Builds on the turbo remote cache (SPEC §12.4) and the working copy (SPEC §9).

## 0. Why asc can do better than each tool's own cache

Each build tool hashes its inputs itself: it reads every source file, often keyed by absolute path and mtime. Under
asc that is wasteful and often wrong:

- **Reading hydrates.** In a virtual workspace a file that nobody opened has no local bytes. Hashing it to make a cache
  key downloads it. asc already knows the identity of every clean file (its `FileId`, blake3) and of every clean
  directory (its `TreeId`) from the tree, without reading anything.
- **The same files live in many places.** Worktrees, stack switches, agents' workspaces and CI checkouts all hold the
  same bytes under different absolute paths. A cache keyed by absolute path or mtime misses on every one of them. A
  key made of repository-relative paths and content ids hits in all of them.
- **A clean directory is one id.** When nothing under `packages/ui` changed, its `TreeId` stands for all of its files:
  the key of a build that reads `packages/ui` costs one lookup, not a walk.

So asc gives every tool the same three things: the identity of its inputs without hydration, one machine-wide store
shared by every worktree and workflow, and the server tier the turbo cache already has.

## 1. Inputs: `asc inputs`

`asc inputs [--json] [--digest] [--allow-missing] [-z] [--] <path>...` prints the content identity of work-tree paths
(relative to the current directory; none means the current directory):

- a directory with no change under it (no modified, added, deleted or untracked path that the ignore rules keep) is
  one entry: `tree <TreeId> <path>`. Nothing below it is read or listed;
- any other file is `file <FileId> <path>`: a clean tracked file takes the id from the tree; a changed or untracked
  file is hashed from its local bytes, which exist because it was written;
- a symlink is hashed as its target text, never followed; ignored and deleted paths are left out; submodules and
  nested repositories are not followed (they are their own repository's inputs); a path that matches nothing is an
  error (exit 1) unless `--allow-missing`;
- the executable bit is the work tree's on Unix and the tree's on Windows, which records none.

`--digest` prints one blake3 (`derive_key("asc build inputs v1")`) over the sorted entries (kind, space, path, NUL,
id). Paths are relative to the
repository root with `/`, so the digest is the same in every worktree that holds the same content. The daemon serves
the same answer over the plugin API (`Inputs` RPC) so editor and build plugins need no child process per call.

Cost: one status walk of the matched paths (the same one `asc status` does, journal-driven in a virtual workspace)
plus a hash of each changed file. No hydration.

## 2. The store

`<ASC_HOME>/cache/build/` on the machine, shared by every repository, worktree and workflow of the user:

```
cas/<2 hex>/<blake3>            output bytes, zstd, written once (temp file + rename)
ac/<2 hex>/<action key>         action entry: the outputs (relative path, mode, size, blob) and metadata
index.redb                      size, last use and origin of each entry, for eviction
```

- **Action key** = blake3 over the tool name, the tool's own key material (version, flags, environment it reads) and
  the input digest of §1. Keys never contain absolute paths. Each adapter (§4) decides its key material and documents
  it.
- **Writes** are atomic and idempotent: a blob is content-named, an action entry is written after its blobs.
- **Eviction**: least recently used down to `cache.build.max_size` (default 20 GiB), run by the daemon's scheduler and
  by `asc cache gc`. Entries in use by a running restore are pinned.
- **Restore** hard-links (or clones, on file systems with reflinks) blobs into place when the output directory is on
  the same volume, and copies otherwise. Restored files are never writable links into the store.

## 3. The server tier

Misses fall through to `asc serve`, reusing the turbo artefact store (`/v8/artifacts`, SPEC §12.4) with its scopes:
reads try the caller's scope and then `trusted`; writes go only to the caller's scope (`trusted` from CI on the
default branch, `pr-<n>`, `dev-<user>`). An action entry and its blobs travel as one artefact (tar, zstd) named by the
action key. Uploads run in the background after the local write. `cache.build.remote = off | read | read-write`
(default `read` for people, `read-write` in trusted CI).

## 4. Adapters

Every adapter uses the same store and server tier. Most tools already speak a cache protocol; the daemon answers
those on the loopback interface. Tools that hash inputs themselves still gain the shared store across worktrees; the
asc-aware adapters also skip hydration.

| Toolchain | Adapter | Inputs from asc |
| --- | --- | --- |
| Rust (Cargo, rustc, `build.rs`) | `asc cache rustc` as `RUSTC_WRAPPER` | yes |
| JS: Rsbuild, Rslib | `@asc/build-cache/rsbuild` plugin | yes |
| JS: Vite | `@asc/build-cache/vite` plugin | yes |
| JS: Turborepo | daemon `/v8` front, then `asc serve` | turbo hashes |
| JS: Nx | daemon Nx remote cache front | Nx hashes |
| Go | `asc cache go` as `GOCACHEPROG` | go hashes |
| C, C++ (ccache) | daemon HTTP storage front (`remote_storage`) | ccache hashes |
| Java, Kotlin (Gradle) | daemon HTTP build cache front | Gradle hashes |
| Java (Maven build cache extension) | daemon HTTP front | Maven hashes |
| Bazel and anything on it | daemon HTTP cache front (`/ac`, `/cas`) | Bazel hashes |
| Anything else (make, scripts, .NET, Python wheels, docs) | `asc cache run` | yes |

### 4.1 Rust

`RUSTC_WRAPPER=asc` (`asc cache rustc`, set by `asc cache setup cargo`):

- **Cacheable**: `lib`, `rlib`, `staticlib`, `proc-macro`, `bin` and build-script compilations without
  `-C incremental`. Anything else runs rustc unchanged. Cargo's dev profile turns incremental on for workspace members;
  `asc cache setup cargo` offers `CARGO_INCREMENTAL=0` for the profiles the person picks, and says what it costs.
- **Key material**: `rustc -vV`, the arguments with every path made repository-relative (and
  `--remap-path-prefix=<root>=.` added so outputs carry no absolute path and are valid in any worktree), the
  environment rustc and the crate read (`CARGO_*` that cargo sets, `env!`/`option_env!` names from the dep-info), the
  extern crates by content id, and the inputs.
- **Inputs without reading the sources (two-level lookup)**: the first compile of a key writes dep-info; the entry
  stores the list of source paths it read. The next lookup takes that list, asks §1 for their ids (tree ids where
  clean) and builds the full key; only on a miss is rustc run. A changed source list is just a different entry.
- **`build.rs`**: compiling the build script is cached as above. Running it is cargo's decision: cargo has no hook
  around a build script run, and its fingerprints use mtimes, so a fresh worktree runs each build script once. What the
  script produces for rustc (its `OUT_DIR` files) is read by the crate's compile, so they are inputs of that compile
  (hashed from local bytes) and the crate's compile still hits.

### 4.2 Rsbuild, Rslib and Vite

`@asc/build-cache` (workspace package `packages/asc-build-cache`) with three entry points:

- **Build-level cache.** The plugin asks the daemon for the input digest of the project: its source directories,
  config files, the lockfile and the workspace packages it imports (from the lockfile's workspace graph), plus the
  tool version, mode and the `define`d environment. On a hit it restores the output directory and ends the build
  before any module is read; on a miss it builds and stores the output. Rsbuild and Rslib use `onBeforeBuild` /
  `onAfterBuild`; Vite uses `config` / `buildStart` / `closeBundle`.
- **Module-level cache.** On a miss, Rsbuild's and Rslib's persistent cache (`performance.buildCache`) is pointed at
  the shared store under a digest of the config and lockfile, with `cacheDigest` set from asc inputs, so a second
  worktree starts warm. Vite's dependency pre-bundle (`cacheDir`) is shared the same way under the lockfile digest.
- **Dev servers are not cached**; only `build`.

### 4.3 Protocols on the daemon

The daemon serves the fronts on `127.0.0.1` only, on a port it chooses, with a random per-user token (Bearer or basic
auth, as each tool supports). Plain HTTP is used only because these tools speak nothing else to a cache, and only on
the loopback interface; it is off until `asc cache setup <tool>` turns it on for that tool (the explicit opt-in the
transport rules ask for). Every front maps the tool's key to an action key with the tool name as prefix, so tools never
read each other's entries.

- Turbo: `/v8/artifacts/*` as `asc serve` does, local first.
- Nx: `PUT|GET /v1/cache/<hash>`.
- Bazel: `GET|PUT /ac/<sha256>`, `/cas/<sha256>`; CAS bodies are checked against their sha256.
- Gradle: `GET|PUT /cache/<key>`.
- Maven build cache extension: `GET|PUT|HEAD` under `/maven/`.
- ccache: `GET|PUT|HEAD|DELETE` under `/ccache/` (ccache's HTTP storage layout).

`asc cache go` speaks `GOCACHEPROG` (JSON over stdio), so Go needs no port.

### 4.4 `asc cache run`

`asc cache run --name <n> --inputs <pathspec>... --outputs <path>... [--env NAME]... -- <command>` keys the command
on its argv, the named environment variables and the inputs (§1), restores the outputs on a hit, and runs the command
and stores the outputs on a miss. Exit status and stdout/stderr are stored with the entry and replayed on a hit.

## 5. Commands

- `asc cache status [--json]`: store size, entries, hit rate per tool over the last day and week, server tier state.
- `asc cache ls [--tool T] [--json]`, `asc cache show <key>`: entries, their origin (worktree, workflow, CI) and the
  other worktrees that used them.
- `asc cache gc [--max-size S]`, `asc cache clear [--tool T]`, `asc cache warm <rev>`: warm fetches the `trusted`
  entries CI wrote for a revision.
- `asc cache setup <cargo|go|ccache|gradle|maven|bazel|nx|turbo|js>`: writes the tool's configuration (user or
  repository level, as asked) and prints what changed. `--undo` takes it out again.

## 6. VS Code

- **Status item**: hits and misses of the last build, and a warning when the server tier is unreachable.
- **Build cache view**: entries by tool and project, size, last use, the worktrees and workflows that share each entry;
  Show, Delete.
- **Tasks**: a task provider for Cargo, Rsbuild, Rslib, Vite and `asc cache run` tasks from `asc.toml`, run with the
  cache on, problems matched into the editor, and the hit or miss shown per task.
- **Admin commands**: Set up caching for this workspace (runs `asc cache setup` for the tools it finds), Warm cache
  for this revision, Clean up cache, Clear cache for a tool.

## 7. Units

1. `asc inputs` and the daemon `Inputs` RPC.
2. `asc-buildcache` crate: store, action entries, eviction; `asc cache status|ls|show|gc|clear`.
3. Server tier on the turbo artefact store; `asc cache warm`.
4. `asc cache run`.
5. Rust: `asc cache rustc`, `asc cache setup cargo`.
6. Go: `asc cache go`, setup.
7. Daemon loopback fronts: turbo, Nx, Bazel, Gradle, Maven, ccache; setup for each.
8. `@asc/build-cache`: Rsbuild, Rslib, Vite.
9. VS Code: status item, view, tasks, admin commands.
10. VS Code open items: TSRX grammar, publishing.
