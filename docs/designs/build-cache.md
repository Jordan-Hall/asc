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

`<ASC_HOME>/buildcache/` on the machine (crate `asc-buildcache`), shared by every repository, worktree and workflow
of the user. It sits beside the object cache, not in it, so `asc gc` of objects never walks it. `asc.buildcache.dir`
moves it.

```
cas/<2 hex>/<FileId>            output bytes as they are, read-only, written once (temp file + rename)
ac/<2 hex>/<action key>         action entry (JSON): the outputs (relative path, kind, size, blob), exit code,
                                captured stdout and stderr blobs, label, creation time and origin
events/<yyyy-mm-dd>.jsonl       one line per hit, miss and write (tool, key, worktree, workflow, bytes), kept 14 days
tmp/                            files being written
```

- **Everything is a file.** `cargo -j` starts one wrapper process per crate and they all use the store at once, so
  there is no database to lock (a single-writer store such as redb would serialise them). The last use of an entry is
  the modification time of its file, touched on every hit. Each event is one append of one line.
- **Blob ids are the asc `FileId`** (blake3, key `asc v1 file`), the same id the work tree has for the same bytes.
  Blobs are stored uncompressed so a restore can clone them.
- **Action key** = blake3 (key `asc build action v1`) over the tool name, the tool's own key material (version, flags,
  environment it reads; each part length-prefixed) and the input digest of §1. Keys never contain absolute paths.
  Each adapter (§4) decides its key material and documents it.
- **Writes** are atomic and idempotent: a blob is content-named, an action entry is written after its blobs and
  refused if one is missing.
- **Eviction**: least recently used down to `asc.buildcache.maxSize` (default 20 GiB); a blob goes when no entry
  refers to it. Runs on `asc cache gc` and, at most once an hour, after a write that leaves the store over the limit.
  A blob or temporary file no entry refers to may belong to a write in progress, so it is kept for an hour.
- **Restore** clones each blob into place where the file system can (reflink on Btrfs, XFS, APFS, ReFS) and copies it
  otherwise, then renames it over the target. Restored files are the caller's own and never links into the store. A
  blob that was evicted between the lookup and the restore makes the restore report a miss, and the tool runs.
- Output paths in an entry are relative with normal components only; anything else is refused on write and restore.

## 3. The server tier

Misses fall through to `asc serve`, reusing the turbo artefact store (`/v8/artifacts`, SPEC §12.4) with its scopes:
reads try the caller's scope and then `trusted`; writes go only to the caller's scope (`trusted` from CI on the
default branch, `pr-<n>`, `dev-<user>`). Under path rules a `trusted` artefact is served only where CI registered its
directory and the caller may build there.

- **Artefact**: an entry and its blobs travel as one artefact named `asc-<key>`:
  `zstd("ASCBC01\n" key[32] u32 len entry-JSON u32 count {id[32] u64 len bytes}*)`. Unpacking stores each blob under
  the id of the bytes received and refuses the artefact when the key differs, a blob's content differs from its id,
  the blobs are not exactly those the entry uses, the entry is over 4 MiB or the whole is over 4 GiB. Entries over
  512 MiB packed stay local.
- **Output safety**: an entry, local or fetched, is refused when an output path is not relative, repeats, or lies
  below another output; when a link target is absolute or climbs above the root (`..` only as a leading run); and a
  restore refuses to write below a directory that is a link.
- **Outbox**: an adapter in `read-write` mode queues each key it puts or hits (`outbox/<key>`) and starts
  `asc cache push` in the background. Push heads each artefact, uploads the ones the server lacks, and posts every
  queued key under the revision (`POST /v8/asc/build-revs/<rev>`, the work tree's HEAD changeset unless `--rev`).
  Each job's list is kept separately and a read merges the caller's scope with `trusted`; lists age out with their
  scope.
- **Warm**: `asc cache warm [<rev>]` reads the revision's keys (`GET /v8/asc/build-revs/<rev>`) and fetches the ones
  the store lacks, eight at a time. A refused artefact is reported and skipped.
- **Mode**: `asc.buildcache.remote = off | read | read-write`; unset it is `read-write` when `CI` is set and `read`
  elsewhere. `off` refuses push and warm; adapters fetch on a local miss in `read` and `read-write`.

## 4. Adapters

Every adapter uses the same store and server tier. Most tools already speak a cache protocol; the daemon answers
those on the loopback interface. Tools that hash inputs themselves still gain the shared store across worktrees; the
asc-aware adapters also skip hydration.

| Toolchain | Adapter | Inputs from asc |
| --- | --- | --- |
| Rust (Cargo, rustc, `build.rs`) | `asc cache rustc` as `RUSTC_WRAPPER` | yes |
| JS: Rsbuild, Rslib | `@asc/build-cache/rsbuild` plugin | yes |
| JS: Vite | `@asc/build-cache/vite` plugin | yes |
| JS: Rstest | `asc-rstest` runner (`@asc/build-cache/rstest`) | yes |
| JS: Turborepo | daemon `/v8` front, then `asc serve` | turbo hashes |
| JS: Nx | daemon Nx remote cache front | Nx hashes |
| Go | `asc cache go` as `GOCACHEPROG` | go hashes |
| C, C++ (ccache) | daemon HTTP storage front (`remote_storage`) | ccache hashes |
| Java, Kotlin (Gradle) | daemon HTTP build cache front | Gradle hashes |
| Java (Maven build cache extension) | daemon HTTP front | Maven hashes |
| Bazel and anything on it | daemon HTTP cache front (`/ac`, `/cas`) | Bazel hashes |
| Anything else (make, scripts, .NET, Python wheels, docs) | `asc cache run` | yes |

### 4.1 Rust

`build.rustc-wrapper = "asc"` (written by `asc cache setup cargo`, or `RUSTC_WRAPPER=asc`): cargo runs
`asc <rustc> <args>…` for each crate, which asc treats as `asc cache rustc -- <rustc> <args>…`. `clippy-driver rustc`
(cargo's workspace wrapper under `cargo clippy`) is accepted the same way, with its lints cached as output.

- **Cacheable**: a compile with one source file, `--crate-name`, `--out-dir`, `dep-info` in `--emit`, and crate types
  among `lib`, `rlib`, `staticlib`, `proc-macro` and `bin`. Anything else runs rustc unchanged: version probes,
  `--print`, `-o`, stdin input, argument files (`@path`), `cdylib` and `dylib`, and `-C incremental`. Cargo's dev
  profile compiles workspace members incrementally; `asc cache setup cargo --no-incremental` writes
  `profile.dev.incremental = false` and says what that costs (an edit recompiles the whole crate).
- **Placeholders**: the work-tree root, the target directory (the ancestor of `--out-dir` with cargo's
  `CACHEDIR.TAG`) and the cargo home are replaced by placeholders in the key, the stored dep-info and the stored output,
  as written natively, with `/` and JSON-escaped (case-insensitive on Windows, and only at a path boundary). A hit puts
  back the current work tree's directories, so cargo's fingerprints and pipelining notices name real files. On a miss
  rustc also gets `--remap-path-prefix=<root>=.`, so the outputs hold no work-tree path.
- **Unit key**: `rustc -vV` (remembered per compiler file unless a rustup proxy could pick another toolchain), the
  working directory, each option with its paths as placeholders, each `--extern` by its file's content, each native
  library directory (`-L native=` and the like) by the content of its files when it is in the target directory or the
  work tree and by name otherwise, the `CARGO_*` variables cargo sets (not the jobserver, target directory or terminal
  ones), `OUT_DIR`, and the variables rustc reads (`RUSTC_BOOTSTRAP`, `SOURCE_DATE_EPOCH`, deployment targets).
  `-L dependency=` is left out: the externs are keyed by content.
- **Full key (two-level lookup)**: the unit key names a `rustc-deps` entry, the sources and variables the dep-info
  listed last time. The full key adds their identity: sources in the work tree through §1 (tree and file ids, no
  hydration), the rest (cargo home, target directory, ignored files) by content, remembered by path, size and time;
  and each listed variable's value. A changed source list is a different entry.
- **Hit**: the outputs (the files in `--out-dir` named after the crate and its `-C extra-filename`) are written, the
  dep-info gets the current directories back, and rustc's output is replayed. **Miss**: rustc runs with its output
  copied through; a success is stored under both keys unless a source changed while it ran. Any failure of the cache
  itself is a warning and rustc runs as usual.
- **`build.rs`**: compiling the build script is cached as above. Running it is cargo's decision: cargo has no hook
  around a build script run, and its fingerprints use mtimes, so a fresh worktree runs each build script once. What the
  script produces for rustc (its `OUT_DIR` files) is read by the crate's compile, so they are inputs of that compile
  (hashed from local bytes) and the crate's compile still hits.
- A compile is not an operation of the repository: `asc cache rustc` writes no op-log entry.
### 4.2 Rsbuild, Rslib and Vite

`@asc/build-cache` (workspace package `packages/asc-build-cache`) caches a whole `build` as one action. It has two
entry points, `@asc/build-cache/rsbuild` (`pluginAscBuildCache`, for Rsbuild and Rslib) and `@asc/build-cache/vite`
(`ascBuildCache`). The plugins call two commands, and any other tool's plugin can call them too:

- `asc cache restore --tool T --name N --inputs P... --outputs P... [--part NAME=VALUE]... [--env V]... [--no-remote]
  [--json]` computes the key: the tool, the project's path in the work tree, the platform, the parts and the named
  environment variables in sorted order, and the digest of the inputs (`asc inputs`). It looks the key up in the local
  store, then on the server tier. An entry counts only when all its blobs are present. On a hit it deletes the output
  paths and restores the entry's outputs. With `--json` it prints `{"hit": bool, "key": hex}` and exits 0; without it,
  it prints the key and exits 1 on a miss.
- `asc cache save --key K --tool T [--name N] --outputs P... [--no-remote]` stores the output paths that exist under
  the key (an error when none exist), queues the entry for the server tier and runs eviction.

What the plugins send:

- **Inputs**: the project directory, each lockfile from the project up to the work-tree root, the workspace packages
  the project depends on (directly or through each other: a dependency whose `node_modules` entry links to a directory
  of the work tree outside any `node_modules`), and the paths in the `inputs` option. Outputs must be ignored by
  `.gitignore` or `.ascignore`, as `dist/` usually is: an output that is an input changes the key on every build.
- **Parts**: the tool version (`api.context.version`, Vite's `this.meta.viteVersion`, and `@rslib/core` under Rslib),
  the mode, the plugin names, the plugin's own version, and a digest of the rest of the configuration (keys sorted,
  functions by their source, the work-tree root replaced by a placeholder, so the same configuration in another work
  tree has the same digest).
- **Environment**: `NODE_ENV`, every variable with a public prefix (`PUBLIC_` for Rsbuild, `envPrefix` or `VITE_` for
  Vite) and the names in the `env` option.

On a hit, the outputs are restored before the tool reads a module. The tool still runs, but does almost nothing:

- Vite builds one empty virtual module with `write: false`.
- Rsbuild and Rslib build one empty entry into `node_modules/.cache/asc-build-cache/<environment>/out`.

On a miss, the tool builds as usual. The plugin saves the outputs after a successful build: in Vite's last
`closeBundle`, and in Rsbuild's `onAfterBuild`.

When the plugin does not run:

- A watched build (`--watch` or `build.watch`) and the dev server are not cached.
- `ASC_BUILD_CACHE=0` or `enabled: false` turns the plugin off.

When the plugin cannot use asc (no binary, or the project is not in a work tree), the build runs as if the plugin were
not there, with one warning. `ASC_BIN` names the binary. `ASC_BUILD_CACHE_DEBUG=1` prints each asc command.
`ASC_BUILD_CACHE_QUIET=1` silences the hit line.

The plugins do not share the tools' module-level caches. Rspack's persistent cache (`performance.buildCache`) and
Vite's `cacheDir` store absolute paths of the work tree, so they cannot be used from another work tree. They also
have no lock, so two work trees writing to the same cache directory would corrupt it. Each work tree keeps its own
module-level cache. The whole-build entry is what different work trees share.

### 4.2.1 Rstest

A test run is cached by a runner, not a plugin: an Rstest plugin or reporter runs only after the tests have started, and
a hit has to skip them. `asc-rstest` (the `bin` of `@asc/build-cache`, also `runRstest` from
`@asc/build-cache/rstest`) takes asc options first, then the rstest command line:

    asc-rstest [--asc-name N] [--asc-output P]... [--asc-input P]... [--asc-env V]... [--asc-no-remote] [--asc-off] run [rstest options]

- It runs the `@rstest/core` installed in the project or a directory above it, with the same Node (or Bun).
- **Cached**: `run` only. Watch mode (`-w`, `--watch`, or no `run`), snapshot updates (`-u`, `--update`, which write
  the sources) and `--asc-off` (or `ASC_BUILD_CACHE=0`) run the tests every time.
- **Key**: the inputs of §4.2 (the project, the lockfiles, the linked workspace packages, `--asc-input`); the parts
  rstest's version, the Node version and the rstest arguments; the variables `NODE_ENV`, `NODE_OPTIONS`, `TZ`, `CI`,
  every `RSTEST_` variable and `--asc-env`.
- **Outputs**: the run's stdout and stderr, kept in `node_modules/.cache/asc-rstest`, and each `--asc-output` (for
  example `coverage`). They must be ignored, as for a build.
- **Hit**: the outputs are restored, the output is replayed and the exit status is 0. No test runs.
- **Miss**: the tests run with their output shown as it comes. Only a run that exits 0, with at most 16 MiB on each
  stream, is stored. A failing run is never stored, so it runs again next time.
- The stored output travels with the entry to the server tier, as with `asc cache run`. A suite that prints secrets
  should run with `--asc-no-remote` (or `asc.buildcache.remote off`), which keeps the entry on the machine.

### 4.3 Protocols on the daemon

The fronts answer the cache protocols of turbo, Nx, Bazel, Gradle, Maven and ccache from the shared store, on
`127.0.0.1` only. Plain HTTP is used only because these tools speak nothing else to a local cache, and only on the
loopback interface; it is off until the user asks for it with `asc cache setup <tool>` or `asc cache env` (the explicit
opt-in the transport rules ask for).

- The front file, `<ASC_HOME>/buildcache-front.json` (mode 0600), holds the port, a 256-bit token and the store. The
  first `asc cache setup <tool>` or `asc cache env` creates it with a free port; later calls keep the port and token,
  because each tool's configuration names them. The file is the user's, so setup for a front tool writes only user
  configuration and refuses `--repo`.
- The daemon serves the fronts while the front file exists; it reads the file every 5 seconds and restarts the
  listener when the port, token or store changes. `asc cache serve` serves them in the foreground for CI or a machine
  without the daemon.
- Every request needs a single loopback `Host` (so a web page that rebinds a name to 127.0.0.1 is refused with 403)
  and the token, as Bearer or as the password of Basic auth (401 otherwise). Keys are printable ASCII, at most 1 KiB,
  with no empty, `.` or `..` path segment. Bodies stream to the store up to the artefact limit of §3.
- Each front keys entries by its tool name, a kind and the tool's key, so tools never read each other's entries, and
  `asc cache ls --tool <tool>` lists them. A `GET` counts a hit or miss for `asc cache status`.
- Under the daemon the fronts never queue entries for the server. `asc cache serve` queues what it stores when the
  server tier is read-write. Neither fetches from the server on a miss; `asc cache warm` fetches ahead of a build.

The routes:

- Turbo: `GET|PUT|HEAD /v8/artifacts/<hash>` (the `x-artifact-tag` signature is kept and returned),
  `/v8/artifacts/status`, `POST /v8/artifacts/events`.
- Nx: `GET|PUT /v1/cache/<hash>`; a `PUT` needs `Content-Length` (411) and an existing hash is 409.
- Bazel: `GET|PUT|HEAD /bazel/ac/<sha256>` and `/bazel/cas/<sha256>`; CAS bodies are checked against their sha256
  (400).
- Gradle: `GET|PUT|HEAD /gradle/<key>`.
- Maven build cache extension: `GET|PUT|HEAD` under `/maven/`.
- ccache: `GET|PUT|HEAD|DELETE` under `/ccache/`, keyed by the last path segment.

What `asc cache setup` writes, each part marked so `--undo` takes out only that:

- turbo, Nx: nothing; they read `TURBO_API`, `TURBO_TOKEN`, `TURBO_TEAM` and
  `NX_SELF_HOSTED_REMOTE_CACHE_SERVER`, `NX_SELF_HOSTED_REMOTE_CACHE_ACCESS_TOKEN` from the environment, which
  `asc cache env [--shell sh|pwsh|cmd|json]` prints and the VS Code extension sets in its terminals and tasks.
- Bazel: a block in `~/.bazelrc` with `build --remote_cache=<front>/bazel` and the token as an `Authorization` header.
- Gradle: `init.d/asc-build-cache.gradle` in the Gradle user home, which reads the front file when a build starts and
  sets an `HttpBuildCache` with push on, and `org.gradle.caching=true` in its `gradle.properties`.
- Maven: a `<server>` with the id `asc-build-cache` and the token in `~/.m2/settings.xml`; the project needs the build
  cache extension, and `asc cache env` gives `MAVEN_ARGS` that turn its remote cache on and point it at the front.
- ccache: `remote_storage` through `ccache --set-config`; setup refuses to replace a value that is not the front's.

`asc cache go` speaks `GOCACHEPROG` (JSON over stdio), so Go needs no port (§4.5).

### 4.4 `asc cache run`

`asc cache run --name <n> [--inputs <path>...] [--outputs <path>...] [--env NAME]... [--no-remote] [-q] -- <command>`
runs any command as a cached action.

- **Key**: the name, the directory it runs in (repository-relative), each argument, each named variable's value (an
  unset variable differs from an empty one), the platform (`os-arch`) and the inputs digest (§1). The inputs default
  to the current directory; ignored files are left out, so outputs below an input do not change the key. No absolute
  path is in the key, so every work tree and workflow with the same inputs shares the entry.
- **Hit**: the outputs are restored below the work-tree root (§2), the stored stdout and stderr are replayed, and the
  stored exit code is returned. `asc cache: hit <name> (<key>)` goes to stderr unless `-q`. An entry whose blobs were
  evicted is a miss.
- **Remote**: on a local miss with the server tier not off, the artefact is fetched (§3). No server, or one that
  cannot be reached, is a miss with a warning; the build goes on.
- **Miss**: the command runs with stdout and stderr copied through as they arrive. Only exit code 0 is stored, with
  the outputs that exist and up to 16 MiB of each stream (more runs uncached). A failure keeps its exit code.
- **Push**: in `read-write` mode the entry (hit or new) is queued and a detached `asc cache push` starts, at most once
  a minute. A CI job runs `asc cache push` itself as its last step, since the job may end before a detached push does.
- On Windows a bare program name is searched on `PATH` with `PATHEXT`, so `npm` finds `npm.cmd`.
### 4.5 Go

`GOCACHEPROG="asc cache go"` (written to the user's go env file by `asc cache setup go`): the go command starts one
`asc cache go` per build and asks it for each compile, link, vet and test result by the go command's own action id.

- **Entries**: tool `go`, keyed by the action id. The entry has one output, named by the go command's output id, whose
  blob is the body; replies name the blob file in the store itself (the go command only reads it), so a hit copies
  nothing.
- **Work trees**: the go command puts a package's directory in its action id unless it builds with `-trimpath`.
  `asc cache setup go --trimpath` adds `-trimpath` to `GOFLAGS`, and then work trees with the same sources share every
  entry. Without it only packages outside the work tree (the module cache, the standard library) are shared.
- **Remote**: a local miss with the server tier on is fetched from the server while the go command asks for other
  actions (replies may come in any order). The first failure to reach the server turns it off for that build, with one
  warning. In `read-write` mode every entry used or stored is queued and pushed when the build ends.
- **Setup**: Go has no repository-level configuration, so `--repo` is refused. `--undo` takes out `GOCACHEPROG` when
  it names asc, and `-trimpath` from `GOFLAGS` only with `--undo --trimpath`.
- The cache server is not an operation of the repository: it writes no op-log entry.
## 5. Commands

- `asc cache status [--json]`: store size, entries, hit rate per tool over the last day and week, and the server
  tier: its mode, the entries waiting in the outbox, and when the last exchange with the server succeeded or why it
  failed (`remote.json` in the store).
- `asc cache ls [--tool T] [--json]`, `asc cache show <key>`: entries, their origin (worktree, workflow, CI) and the
  other worktrees that used them.
- `asc cache gc [--max-size S]`, `asc cache clear [--tool T]`, `asc cache rm <key>...` (keys or unique prefixes; an
  unknown key removes nothing).
- `asc cache push [--rev R] [--remote N]`, `asc cache warm [<rev>] [--remote N]`: send the outbox and list it under a
  revision; fetch a revision's entries before a build.
- `asc cache run --name <n> ... -- <command>`: §4.4.
- `asc cache restore ...` and `asc cache save --key K ...`: one build as a cached action, for a build tool's plugin
  (§4.2).
- `asc cache serve`: the fronts in the foreground (§4.3). `asc cache env [--shell sh|pwsh|cmd|json]`: the front's variables for turbo, Nx and Maven.
- `asc cache setup <cargo|go|ccache|gradle|maven|bazel|nx|turbo> [--user|--repo] [--no-incremental] [--trimpath] [--undo]`: writes the tool's configuration (user or
  repository level, as asked) and prints what changed. `--undo` takes it out again. The Rsbuild, Rslib and Vite
  plugins go in the build's own configuration (§4.2).

## 6. VS Code

The extension runs `asc cache ... --json` directly, with the fixed argv of `cli-direct` (no shell, typed inputs, a
time limit), and reads each reply defensively. It refreshes when a task ends, when the view is shown, when the window
gets focus and after an admin command; there is no timer. A failed read hides the status item and is not logged: the
cache is optional.

- **Status item**: the hits and misses of the last `asc-build` task (the difference of `cache status` before and after
  it), else today's hit rate; the tooltip has each tool's numbers, the size of the store and the server tier's mode,
  queue and last exchange. A failed last exchange turns it into a warning. A click focuses the view.
- **Build cache view** (`asc.buildCache`): tools, then their entries (label, size, last use; the tooltip names the
  work tree and workflow that wrote it). Show opens `cache show --json` as a read-only document, with the work trees
  and workflows that used the entry; Delete runs `cache rm` after a confirmation; Clear on a tool runs
  `cache clear --tool`.
- **Tasks** (type `asc-build`, trusted workspaces only): `cargo build` with asc as `RUSTC_WRAPPER` (and
  `CARGO_INCREMENTAL=0`), `go build ./...` with asc as `GOCACHEPROG`, and each package.json script that runs
  `rsbuild build`, `rslib build` or `vite build`, with the package manager of the lockfile (`@asc/build-cache` in the
  config caches it). There is no `asc.toml`: an `asc cache run` task is declared in tasks.json with `name`, `inputs`
  (default the root), `outputs`, `command` and `env`; paths must stay inside the root. Every task gets the variables
  of `cache env`, and the problem matchers `$asc-rustc`, `$asc-go` and `$asc-build`.
- **Admin commands**: Set up build caching for this workspace (offers the tools whose files it finds, runs
  `cache setup` for each one picked), Warm build cache for this revision, Clean up build cache (`cache gc`), Clear
  build cache (a tool or all, after a confirmation), Refresh.
- **Terminals**: new terminals get the variables of `cache env` (`asc.cache.terminalEnv`, default on), so turbo, Nx
  and Maven run there use the local fronts.

## 7. Units

1. `asc inputs` and the daemon `Inputs` RPC.
2. `asc-buildcache` crate: store, action entries, eviction; `asc cache status|ls|show|gc|clear`.
3. Server tier on the turbo artefact store; `asc cache warm`.
4. `asc cache run`.
5. Rust: `asc cache rustc`, `asc cache setup cargo`.
6. Go: `asc cache go`, setup.
7. Daemon loopback fronts: turbo, Nx, Bazel, Gradle, Maven, ccache; setup for each.
8. `@asc/build-cache`: Rsbuild, Rslib, Vite, Rstest.
9. VS Code: status item, view, tasks, admin commands.
10. VS Code open items: TSRX grammar, publishing.
