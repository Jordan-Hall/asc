# Dependency snapshots

Status: design, not built. Roadmap row 22 (`docs/ROADMAP-NEXT.md:68`). `node_modules` snapshots
are listed as out of scope in `docs/SPEC.md:52`; the owner decided on 2026-11-13 to build them, with
this design first. The SPEC entry moves from the out-of-scope list to a new section when unit 13
lands.

## Why

A JavaScript install downloads tarballs, unpacks tens of thousands of small files, links them and
may run lifecycle scripts. The result depends only on a small set of inputs: the lockfile, the
package manager and its configuration, the platform and the install flags. Every CI job and every
new checkout repeats the same work for the same inputs.

A dependency snapshot stores the installed result once, as a tree in the asc object store, under a
key computed from those inputs. A later install with the same key mounts or clones the tree
instead of downloading and unpacking. Files that do not change between two lockfile versions are
stored once, because they have the same `FileId`.

Non-goals:

- asc never resolves dependencies, never talks to a package registry for this feature and never
  runs an install on the server. The package manager stays the only thing that decides what is
  installed.
- A snapshot is not a build cache. `node_modules/.cache` and `node_modules/.vite` are build output
  and belong to the build cache (`crates/asc-buildcache`) and to `depclone`.
- Package manager stores outside the work tree (`~/.pnpm-store`, `~/.bun/install/cache`,
  `~/.npm/_cacache`) are not captured.

## Current state

What exists, and what each piece gives this design:

| Piece | Where | What it gives |
|---|---|---|
| Lockfile list | `crates/asc-wc/src/depclone.rs:18` (`LOCKFILES`) | The six JavaScript lockfile names, reused as the key's lockfile set. |
| `node_modules` search | `depclone.rs:28` (`MAX_DEPTH = 6`), `:31` (`SKIP`), `:161` (`find_node_modules`) | The rule for which `node_modules` directories belong to one install. |
| Lockfile comparison | `depclone.rs:124` (`lockfiles_match`) | The current "same dependencies" test: lockfiles and each `package.json` beside a `node_modules`, byte for byte. The key generalises it. |
| Reflink clone | `depclone.rs:70` (`clone_deps`), `:213` (`volume_reflinks`), `:226` (`can_reflink`) | Whole-directory reflink into a temporary name, then one rename. Restore uses the same pattern. |
| Materialise one file | `crates/asc-wc/src/plain.rs:520` (`materialise`) | Reflink from the store's loose file, fall back to a copy; symlinks never followed; exec bit. |
| Large files | `plain.rs:583` (`assembled_path`), `:596`, `:605` (`publish_assembled`), `:759` (`write_chunked`) | One assembled copy per large file per machine, reflinked into each checkout. |
| Writing under a root | `plain.rs:438` (`write_path`), `:449` (a symlink at the destination is replaced, never followed) | Restore writes the same way. |
| Path safety | `crates/asc-wc/src/safe_path.rs:19` (`try_fs_path`), `:61` (`parents_are_real_dirs`), `:88` (`case_collisions`), `:134` (`Verified`), `:221` (`SafeRoot`) | No write below a symlinked ancestor, no case-colliding siblings, no unrepresentable names. |
| Path rules | `crates/asc-core/src/path.rs:153` (`windows_valid`), `:230` (`checkout_safe`) | git's `verify_path` with NTFS and HFS protection, applied to every snapshot path. |
| Tree objects | `crates/asc-core/src/object.rs:133` (`Kind`), `:143` (`Entry`), `:179` (`Tree::new`, empty only at the root) | The stored form. |
| Chunking | `crates/asc-core/src/content.rs:17` (`CHUNK_THRESHOLD = 4 MiB`), `crates/asc-store/src/key.rs:13` (`ObjKey::ChunkList`, `ObjKey::Chunk`) | Large files are content-defined chunks, shared across versions. |
| Tree diff | `crates/asc-engine/src/tree.rs:549` (`leafdiff`) | Incremental restore from one snapshot to the next. |
| Redirections | `crates/asc-wc/src/redirect.rs:16` (`DEFAULT_NAMES` holds `node_modules`), `crates/asc-vfs/src/core.rs:92`, `:292` | `node_modules` is never tracked and is hidden from virtual worktrees today. |
| Virtual mounts | `crates/asc-vfs/src/core.rs:80` (`Compose`), `:85` (`Options`), `:203` (`CoreFs`), `:639` (`Provider`), `crates/asc-vfs/src/compose.rs:51` (`Projection`), `:79` (`Composition`) | A tree projected at a path inside a mount; the model for a mounted snapshot. |
| Actions store | `crates/asc-ci/src/cache.rs:80` (`StoreKind`), `:96` (`ActionsManifest`), `:138` (`validate`), `:228` (`ManifestApi`), `:776` (`save`), `:817` (`restore`), `:673` (`materialise`) | Keyed, immutable manifests over ordinary objects, validated before any write. |
| Scopes and retention | `crates/asc-serve/src/actions_store.rs:1-13`, `:408` (`retention`), `:738` (`ActionsRoots`); `crates/asc-serve/src/turbo.rs:173` (`Scope`), `:227` (`ScopeSource::scope`), `:455` (`caller_scope`) | `trusted` is written only by an Actions task on a push to a protected branch; a pull request writes `pr-<n>`; reads try the caller's scope, then `trusted`. |
| GC roots | `crates/asc-store/src/gc.rs:159` (`GcRootSource`) | An unexpired record keeps its tree alive. |
| Key hashing | `crates/asc-buildcache/src/key.rs:7` (`KEY_CONTEXT`), `:16` (`builder`), `:50` (`part`), `:61` (`finish`) | Length-prefixed, named, ordered BLAKE3 key parts; the snapshot key uses the same construction with its own context. |
| Signatures | `crates/asc-core/src/sshsig.rs:19` (`NAMESPACE`) | SSHSIG verification with fixed namespaces; a new namespace signs snapshot records. |
| Lockfile parsers | `crates/asc-serve/src/deps/mod.rs:10-19`, `deps/parse/mod.rs:35` (`MAX_FILE_BYTES`), `:42` (`Format`), `:226` (`parse_file`), `deps/parse/npm.rs` | Pure, bounded parsers that never run repository code. The server uses them to check a record. |
| Build cache plugins | `packages/asc-build-cache/src/core.ts:1-3` | The lockfile is already a build input there; a restored snapshot leaves it unchanged, so build keys stay valid. |

`asc worktree add` calls `clone_deps` (`crates/asc/src/cli/worktree.rs`). It gives nothing when
the volume cannot reflink or when the lockfiles differ (`Skipped::NoReflink`,
`Skipped::LockfilesDiffer`). Snapshots fill both gaps.

## The key

A `SnapshotKey` is 32 bytes:

```
BLAKE3 derive_key("asc deps snapshot v1",
    part("format", "1")
    part("ecosystem", "npm")
    part("root", <project root, repository-relative, "/"-separated, "" for the root>)
    part("manager", "npm" | "pnpm" | "yarn" | "bun")
    part("manager_version", <exact version, for example "9.12.0">)
    part("lockfiles", <digest of each LOCKFILES name at the root: name and BLAKE3 of bytes>)
    part("manifests", <digest of every package.json of the install: path and BLAKE3 of bytes>)
    part("config", <digest of the install configuration files, scrubbed>)
    part("os", "linux" | "darwin" | "win32" | "freebsd")
    part("arch", "x64" | "arm64" | ...)
    part("libc", "glibc-<major>.<minor>" | "musl" | "none")
    part("node_abi", <process.versions.modules, for example "127"> | "none")
    part("install", <the install argv after the program name, NUL-separated>)
    part("scripts", "none" | "allow:<BLAKE3 of the sorted allow list>" | "all")
    part("env", <sorted NAME=value pairs of the names listed in the configuration>))
```

`part` is `KeyBuilder::part` of `crates/asc-buildcache/src/key.rs:50`: the name and the value are
each length-prefixed, so no two sequences of parts hash alike. A digest of several files hashes
the sorted list of `(path, BLAKE3 of bytes)` pairs, so the order in which files are found does not
matter. The key is written as 64 hex characters.

Why each input is in it:

- **format, ecosystem.** A change to capture or restore rules, or another ecosystem (see
  "Beyond npm"), must never meet an old snapshot.
- **root.** One repository can hold several installs (a monorepo with two package managers). The
  root also fixes where workspace links point.
- **manager and manager_version.** npm, pnpm, yarn and bun lay out `node_modules` differently, and
  a minor version can change the layout (pnpm's virtual store naming, npm's hidden lockfile
  format). The version comes from the `packageManager` field of the root `package.json` when it
  pins one (corepack form `pnpm@9.12.0+sha512...`, the hash is dropped), otherwise from
  `<manager> --version` on the machine that runs the install.
- **lockfiles.** They pin every resolved version and integrity hash. They are hashed, never
  parsed, so `bun.lockb` (binary) needs no parser.
- **manifests.** Workspace membership, `bin` entries, `dependenciesMeta`, `trustedDependencies`,
  `pnpm.onlyBuiltDependencies`, `overrides` and `patchedDependencies` live in `package.json` files
  and change the installed tree without always changing the lockfile. The set is the `package.json`
  at the root and every `package.json` found by the `find_node_modules` walk rules
  (`MAX_DEPTH`, `SKIP`), outside `node_modules`. Whole bytes are hashed; see open question 1.
- **config.** `.npmrc`, `.yarnrc`, `.yarnrc.yml`, `pnpm-workspace.yaml`, `.pnpmfile.cjs`,
  `bunfig.toml`, and every file under `patches/`, `.yarn/patches/`, `.yarn/plugins/` and
  `.yarn/releases/`, plus the `inputs` globs of the configuration. They choose the linker
  (`nodeLinker`, `node-linker`), hoisting and patches. Before hashing, lines whose key contains
  `_auth`, `_authToken`, `_password`, `username`, `email`, `certfile`, `keyfile` or `cafile` are
  removed, so a credential never enters a key and a rotated token does not miss.
- **os, arch, libc.** Packages declare `os`, `cpu` and `libc` fields, and managers install only
  the matching optional dependencies (for example a bundler's per-platform binary). Native addons
  built on a newer glibc do not load on an older one. Node's names are used because the `os` and
  `cpu` fields use them. libc is read from the `PT_INTERP` of the `node` executable's ELF header (a
  pure parse, nothing runs); the glibc version from the `GNU C Library` version string in the
  `libc.so.6` the interpreter names. Not Linux: `none`.
- **node_abi.** Addons compiled by node-gyp, and prebuilt addons chosen by `prebuild-install`, are
  bound to `NODE_MODULE_VERSION`. Read with `node -p process.versions.modules` on the machine that
  runs the install. `none` when no `node` is on `PATH` (a bun-only project).
- **install.** `--omit=dev`, `--production`, `--no-optional`, `--filter`, `--ignore-scripts` and
  the rest change the tree. The argv is taken as given in the configuration; asc does not try to
  normalise equivalent spellings.
- **scripts.** Whether lifecycle scripts ran changes the tree (compiled addons, downloaded
  binaries, generated files). See "Producing a snapshot".
- **env.** Some installs read environment variables (`NODE_ENV`, a manager's `*_CONFIG_*`
  variables). Only names listed in the configuration are read. A listed name that matches
  `*TOKEN*`, `*SECRET*`, `*PASSWORD*` or `*AUTH*` (any case) is refused.

Not in the key, on purpose:

- The registry URL. Integrity hashes in the lockfile pin the bytes, so a mirror gives the same
  tree.
- The absolute path of the work tree. Snapshots must be relocatable; capture checks that they are
  (see "Stored form").
- The time, the user name and the host name.

The record keeps every part in clear text except digests of file contents, so `asc deps key
--explain` can say which input differs between two keys.

## Configuration

`.asc/snapshots.toml`, at most 64 KiB, read from the revision being installed:

```toml
[[snapshot]]
name = "web"                                 # shown in listings; [A-Za-z0-9_-]{1,64}
root = "."                                   # project root holding the lockfile
manager = "pnpm"                             # optional; else from lockfile and packageManager
install = ["pnpm", "install", "--frozen-lockfile"]
scripts = "allow"                            # "none" | "allow" | "all"
allow = ["esbuild", "sharp"]                 # packages whose lifecycle scripts may run ("allow")
env = ["NODE_ENV"]                           # environment variables that are key parts
inputs = ["tools/install/*.json"]            # more files hashed into the "config" part
exclude = ["node_modules/.cache", "node_modules/.vite"]   # never captured (these two are default)
```

Without the file, `asc deps restore` and `asc deps save` work on the repository root with the
detected manager, `install` set to the manager's frozen install
(`npm ci`, `pnpm install --frozen-lockfile`, `yarn install --immutable`,
`bun install --frozen-lockfile`) and `scripts = "all"`, which is what those commands do.

For a write to `trusted`, the `publishers` list (see "Trust") is read from the protected branch tip,
never from the revision under test, as `.asc/deps.toml` is read from trunk
(`crates/asc-serve/src/deps/mod.rs:14`).

## Stored form

A snapshot is two things in the repository's namespace:

1. **A tree.** An ordinary `Tree` (`crates/asc-core/src/object.rs:179`) rooted at the project
   root, holding only the snapshot roots of the install (below). Entries are `File`, `Exec`,
   `Symlink` and `Dir`; never `Gitlink`. Every entry carries its git SHA-1 like any tree entry,
   computed during capture, because the server recomputes and verifies client-supplied git fields
   (SPEC §0 decision 4).
2. **A record.** A `SnapshotRecord` (postcard, at most 1 MiB) in the Actions store under the new
   `StoreKind::Snapshot` (`snapshot` namespace): the key, the clear-text key parts, the root
   `TreeId`, counts, the empty directories, the producer and an optional signature.

```rust
pub struct SnapshotRecord {
    pub version: u8,                       // 1
    pub key: [u8; 32],
    pub parts: Vec<(String, String)>,      // name, clear text or digest hex
    pub tree: TreeId,
    pub files: u64,
    pub bytes: u64,                        // logical size, sum of file sizes
    pub empty_dirs: Vec<RepoPath>,         // non-root trees cannot be empty
    pub host_bound: Vec<RepoPath>,         // files whose root path was replaced on capture
    pub scripts_declared: Vec<String>,     // packages that declare install scripts
    pub external_writes: Vec<String>,      // writes outside the install root, see "Producing"
    pub producer: Producer,                // asc version, changeset, workflow, runner image
    pub created: u64,
    pub signature: Option<String>,         // armored SSHSIG, see "Trust"
}
```

Snapshot roots by layout:

| Layout | Roots captured |
|---|---|
| npm, bun, yarn `node-modules` linker | every `node_modules` found by the `find_node_modules` rules |
| pnpm | the same; `node_modules/.pnpm` (the virtual store) is inside the root `node_modules` |
| yarn PnP | `.pnp.cjs`, `.pnp.loader.mjs`, `.pnp.data.json`, `.yarn/unplugged/`, `.yarn/install-state.gz`; `.yarn/cache/` only when it is not tracked |

**Chunking.** Files over `CHUNK_THRESHOLD` (4 MiB) are chunked like any file, so a large native
binary that is the same in two snapshots is stored once, and one that changes by a few bytes shares
most chunks. Delta storage (`docs/design/DELTA-STORAGE.md`) applies to snapshot files and trees on
the server like to any loose object, because they are ordinary objects.

**Sharing.** Two snapshots that differ by one dependency share every other file by `FileId` and
every unchanged directory by `TreeId`. pnpm's layout shares best: each
`.pnpm/<name>@<version>/` directory is the same tree in every snapshot that holds that version. In
a hoisted npm layout a version change moves files between directories, so files are shared but
fewer trees are.

**Relocatability.** Capture scans every file of at most 1 MiB, and every file of the host-bound
list whatever its size, for the absolute install root (and the package manager's store path when
known). Host-bound state files (`node_modules/.modules.yaml`, `node_modules/.yarn-state.yml`) get
each occurrence replaced by the placeholder `\u{0}asc-root\u{0}`, and restore writes the local root
back. Any other file that contains the root makes the snapshot non-relocatable: it is kept only in
the machine-local tier, under a key with one more part `part("abs_root", <path>)`, and is never
uploaded. `asc deps save` names the file.

**What a tree cannot hold.** Modification times, owners and modes other than the exec bit are not
stored. Hard links (pnpm links files from its store) are stored as separate entries with the same
`FileId`, so they cost nothing extra in the store and restore as independent files. Sockets, FIFOs
and device files make capture fail with the path named.

**Retention and GC.**

- Server: records follow the scope retention of the Actions store (`actions_store.rs:11`: `pr-*`
  and `dev-*` 14 days, `trusted` 60 days, by last access). An unexpired record is a GC root for its
  tree through `ActionsRoots` (`actions_store.rs:738`), which today roots `FileId`s and gains
  `ObjKey::Tree` roots. A per-scope cap `[deps.snapshots] max_bytes_per_scope` (logical bytes,
  default 50 GiB) evicts the least recently read records first.
- Machine: fetched snapshot objects go to the evictable `cache/` (SPEC §0 decision 5), never to
  `store/`, because an install can always make them again. `<asc home>/deps/index/<key hex>`
  holds the record; `<asc home>/deps/assembled/<key hex>/` may hold one materialised copy for
  whole-tree reflinks (see "Consuming"). `asc gc` evicts both by last use under
  `asc.deps.cacheSize` (default 20 GiB).

**Size limits.** A record is refused, on capture, on upload and on restore, beyond:

| Limit | Value | Reason |
|---|---|---|
| Files | 1,000,000 | A large monorepo install is a few hundred thousand files. |
| Logical bytes | 16 GiB | Larger installs are better served by the package manager's own store. |
| One file | 1 GiB | `MAX_FILE_BYTES` of `crates/asc-ci/src/cache.rs:35`. |
| Symlink target | 4,096 bytes | `MAX_LINK_BYTES`, `cache.rs:39`. |
| Path | 4,096 bytes, 64 components | Bounds the walk; deeper nesting is refused, not truncated. |
| Empty directories | 100,000 | Bounds the record. |
| Record | 1 MiB | Postcard; decoded with a length check and no trailing bytes, like `ActionsManifest::decode`. |

## Producing a snapshot

**Who runs the install.** A CI job or a developer's machine, never the server. The server keeps
the rule of `crates/asc-serve/src/deps/mod.rs:10`: nothing of the repository runs there. `asc deps
install` runs the configured argv as a child process with no shell, in the project root, then
captures.

**Capture.**

1. Compute the key before the install (all inputs are files or probes), so a hit skips the install.
2. Run the install. With `scripts = "allow"` asc passes the manager's own switch for it
   (`--ignore-scripts` plus the manager's allow list mechanism); asc does not intercept scripts
   itself.
3. Recompute the key. A lockfile or `package.json` the install rewrote (a non-frozen install)
   changes the key; capture then refuses, because the snapshot would not match the committed
   inputs.
4. Walk the snapshot roots with the `safe_path` rules: no following symlinks, `checkout_safe`
   on every path, `windows_valid` names on Windows, the exclude list, the limits. Directory
   junctions on Windows are read as symlinks; an absolute target inside the root is stored as a
   relative target, one outside makes capture fail.
5. Store each file through `Objects` (reflink-friendly: content already in the store is not
   read twice), build the trees bottom-up, compute git SHA-1s.
6. Run the relocatability scan, list `scripts_declared` (the `preinstall`, `install` and
   `postinstall` fields of each captured `package.json`, a pure JSON read) and write the record.
7. Upload missing objects, then put the record. Keys are immutable: a second put of the same key
   in the same scope returns "exists" and uploads nothing, as `cache::save` does (`cache.rs:776`).

**Lifecycle scripts and reproducibility.** A lifecycle script is arbitrary code. It can download
binaries, read the network, the clock and the environment, and write outside `node_modules`. A
snapshot freezes one outcome of it. That is consistent (every consumer gets the same bytes) but not
reproducible (a second install may differ). Rules:

- With `scripts = "none"` an install is close to a pure function of the lockfile, and two
  independent producers should get the same `TreeId`. `asc deps verify --against <key>` compares.
- `asc deps install` runs the install with `HOME`, `XDG_CACHE_HOME`, `TMPDIR` (and on Windows
  `USERPROFILE`, `LOCALAPPDATA`, `TEMP`) pointed at a scratch directory beside the work tree. What
  is left there afterwards is listed in `external_writes`. A browser download into the home
  cache, for example, is not in the snapshot, so a consumer would miss it. A record with
  `external_writes` is refused for `trusted` unless the configuration sets
  `allow_external_writes = true`, and `asc deps restore` prints the list.
- `scripts_declared` tells a reviewer which packages could have changed the tree.

## Trust

A snapshot is executable code. Restoring one is the same as running an install whose result
someone else chose. The model:

**Who may publish for a key.** A write goes only to the caller's scope (`turbo.rs:173`,
`actions_store.rs:8-10`). `trusted` is written only by an Actions task on a push to a protected
branch; a pull request writes `pr-<n>`; a person writes `dev-<principal>`. Reads try the caller's
scope, then `trusted`. So a pull request can poison only its own scope, which already runs its own
code, and a developer only their own. A job in `trusted` never reads another scope.

**Provenance.** The server stamps the scope, the principal and the time beside the record (not
client-supplied fields). The record carries the producer's asc version, the changeset of the
revision installed, the workflow run and the runner image name. `asc deps ls --long` shows them.

**Server checks on write**, all pure and bounded:

- The record decodes and validates (limits, every path `checkout_safe`, every symlink target
  passes the restore rules below, `empty_dirs` sorted and unique).
- The tree and every object it names are present (the upload finished).
- For the `lockfiles` and `manifests` parts: the server reads those files at the record's
  changeset (`ReadFile`, no checkout), hashes them and refuses a mismatch. A producer cannot store
  a tree under a key that claims inputs the revision does not have.

**Signatures.** Optional. A producer with a key signs the SSHSIG message
`asc-deps-snapshot-v1\n<key hex>\n<tree id hex>\n<BLAKE3 hex of the record without signature>\n` in
the new namespace `asc-deps-snapshot` (added beside `NAMESPACE` in `crates/asc-core/src/sshsig.rs`).
`publishers = ["ssh-ed25519 ..."]` in `.asc/snapshots.toml` at the protected branch tip lists the
keys a consumer accepts; `asc config asc.deps.requireSigned true` makes an unsigned or unknown
signature a miss. Signatures matter when the storage is not the server's own bucket: a
bring-your-own S3-compatible bucket, a shared read-only host cache, a mirror.

**Against poisoning.**

| Attack | Answer |
|---|---|
| A pull request writes a bad tree for trunk's key | Scope rules: it can only write `pr-<n>`. |
| A record claims inputs it does not have | Server recomputes the lockfile and manifest digests at the changeset; the consumer recomputes the whole key locally and refuses a record whose `key` differs. |
| A tree that does not match the lockfile | `asc deps verify` checks layout against the lockfile (unit 10); trusted CI can run it after `save`. |
| Tampered objects in storage | Every object is verified against its id on read. |
| Tampered record in storage | Signature, when required. |
| A compromised dependency in trunk | The snapshot is as dangerous as the install, no more. `asc deps purge --key <k>` and `asc deps purge --before <time>` (maintainers) delete records so the next job installs again. |
| A hostile tree that writes outside the root | Restore rules below; checked on the server and again on the client. |

## Consuming

**Restore order.** `asc deps restore` computes the key, then looks in: the marker
`<root>/node_modules/.asc-snapshot` (key and tree id of what is there; same key means nothing to
do), the machine index, the server (caller's scope, then `trusted`). A miss exits with status 3
and changes nothing, so a script can run the install.

**Incremental restore.** When the marker names an earlier snapshot of the same root, restore
writes only `leafdiff(old tree, new tree)` (`crates/asc-engine/src/tree.rs:549`), as a plain
switch does. A file changed locally since the restore (a different size or mtime than the marker
recorded) makes restore fall back to a full restore of that root. `--check` hashes every file
instead.

**Three ways to materialise**, chosen per volume:

1. **Virtual mount** (virtual worktrees only). The snapshot tree is projected at each snapshot root
   inside the mount, beside the worktree tree, through a new overlay hook in `asc-vfs` like
   `Compose` (`crates/asc-vfs/src/core.rs:80`). Redirections keep `node_modules` out of status as
   today. Files are hydrated on first read. Writes copy up into the worktree's local state, the
   same way an edit of a tracked file does in that backend, and the marker records that the root
   is dirty. Hydration prefetches every `package.json`, every `*.node` and everything under
   `.bin`, because the loader reads those first and `dlopen` needs a whole file.
2. **Reflink clone.** The tree is written into a temporary directory beside the root
   (`<root>.asc-restore-<pid>`), then renamed over it, as `clone_deps` does (`depclone.rs:70`).
   Each file comes from the store's loose file by reflink (`plain.rs:520`) and each large file from
   its assembled copy (`plain.rs:583`). When `<asc home>/deps/assembled/<key>/` exists on the same
   volume, the whole root is reflinked from it with the `depclone` walk instead, which costs only
   metadata.
3. **Plain copy.** On a volume that cannot reflink, bytes are copied. Unlike `depclone`, which
   never makes a byte copy, this is no worse than the install it replaces: the install would
   write the same bytes. Hard links are never used: an edit through one would change the store.

**Symlink layouts.**

| Manager | Layout | Restore |
|---|---|---|
| npm | hoisted tree; `.bin/*` relative symlinks on Unix, `.cmd` and `.ps1` shims on Windows; workspace packages are links to `../../packages/<name>`; `node_modules/.package-lock.json` hidden lockfile | npm trusts the hidden lockfile only when it is newer than every package directory, so restore sets its mtime last, after every directory. |
| pnpm | `node_modules/.pnpm/<name>@<version>/node_modules/<name>` real directories; top-level and dependency links into `.pnpm`; `.modules.yaml` state with absolute paths; junctions on Windows | `.modules.yaml` is host-bound (placeholder replaced with the local root and store). Directory links on Windows become junctions. |
| bun | hoisted like npm, or isolated like pnpm with the isolated linker; `.bin` links | Same as npm or pnpm. |
| yarn PnP | no `node_modules`; `.pnp.cjs` maps packages to zip archives in `.yarn/cache` and directories in `.yarn/unplugged` | The PnP roots of the table in "Stored form". `.yarn/cache` is usually tracked and then comes from the checkout. |
| yarn `node-modules` | hoisted; `.yarn-state.yml` | `.yarn-state.yml` is host-bound. |

On Windows a symlink entry whose target is a directory (in the snapshot tree or in the work tree)
is made as a junction with an absolute target, which needs no symlink right; a file link needs the
right (Developer Mode or `SeCreateSymbolicLinkPrivilege`) and otherwise fails the restore, which
then falls back to the install.

**Tools that write into `node_modules`.** Build caches (`.cache`, `.vite`) are excluded from
capture and survive an incremental restore, because restore removes only paths the old snapshot
had. A tool that rewrites a package file (a patch step after install) changes the tree; the next
`asc deps restore` puts the snapshot back, and the patch step belongs in the install (as a
lifecycle script) so that it is captured. The package manager itself, run after a restore, sees a
complete install and does nothing, which is the point; `asc deps restore` then rewrites the marker
only if the tree changed.

**Native modules.** `*.node` files are ordinary `Exec` or `File` entries. The key's `os`, `arch`,
`libc` and `node_abi` parts keep incompatible builds apart. A restored addon is a private file
(clone or copy), so `dlopen` never maps a file in the store.

## Protocol, CLI and CI

**Protocol.** Over https, through the Actions store endpoints already used by `asc ci cache`:
`PUT|GET /actions/v1/<owner>/<repo>/snapshot/<pct(key hex)>` and the listing under the same
prefix. Objects move through `Objects` like any repository content. ssh carries snapshots once the
asc RPC protocol over ssh exists (it is out of scope today, `docs/SPEC.md:52`). There is no plain
http transport. The wire never carries a snapshot as an archive: the tree and its objects are
fetched like a checkout's, with the existing batch fetch, so content already in the machine store
is never sent again.

**CLI.** The new verbs join the `asc deps` group (`crates/asc/src/cli/deps/mod.rs`). They are the
first `asc deps` verbs that need a work tree; the help says so.

```text
asc deps key [--name N] [--explain [OTHER-KEY]]     print the key and its parts, or which differ
asc deps restore [--name N] [--mode auto|virtual|clone|copy] [--check] [--no-remote]
asc deps save [--name N] [--no-upload] [--sign KEYFILE]
asc deps install [--name N] [-- ARGV...]            restore; on a miss run the install, then save
asc deps verify [--name N] [--against KEY]          layout against the lockfile; tree against a key
asc deps ls [--name N] [--long]                     records the caller can read, newest first
asc deps purge (--key K | --before TIME) [--scope S]
```

Exit status of `restore`: 0 restored or already current, 3 miss, 1 error. `install` exits with
the install's status when it ran one. `save` after a non-frozen install that changed a key input
exits 1 and names the file.

**CI.** One step replaces the install step:

```yaml
- run: asc deps install
```

On a trusted push it restores or installs and saves into `trusted`. In a pull request it reads
`trusted`, installs on a miss and saves into `pr-<n>`, so the second run of the same pull request
hits. The step prints the key, the hit or miss, the scope that served it and the time saved, and
writes the same to the job summary. A server that cannot be reached is a miss with a warning, never
a failure: the install runs as before. `asc ci checkout` exports `ASC_DEPS_KEY_<NAME>` for jobs that
want to key other caches on the dependency set.

**Fallback.** Every failure on the restore path (miss, unreachable server, failed verification,
refused path, case collision, missing symlink right, limit) leaves the snapshot root as it was or
absent, never half-written, and `asc deps install` runs the normal install.

**Worktrees.** `asc worktree add` tries `clone_deps` first, as today. When it reports
`NoReflink` or `LockfilesDiffer`, it runs `asc deps restore` for each configured snapshot, quietly,
and reports the result in the same line.

## Beyond npm

The key, the record, capture, restore and the trust model are ecosystem-neutral. An ecosystem
supplies four things: its key parts, its snapshot roots, its host-bound files and its verify
check. None of these is built now; the `ecosystem` key part and the `Ecosystem` enum leave room.

| Ecosystem | Roots | Key parts beyond the common ones | Host-bound |
|---|---|---|---|
| cargo vendor | `vendor/` | `Cargo.lock` digest, cargo version; no `node_abi` | `.cargo/config.toml` source replacement is in the repository, not in the snapshot |
| pip and uv | `.venv/` | lockfile (`uv.lock`, `requirements*.txt` with hashes), Python version and ABI tag (`cp312`), platform tag | `pyvenv.cfg`, script shebangs and `activate` scripts hold absolute paths; uv's relocatable venvs avoid most of them |
| Go modules | a module cache directory below the work tree, pointed at by `GOMODCACHE` | `go.sum` digest, Go version | none; the module cache is content-addressed and read-only |

The build cache stays responsible for build output (`target/`, `__pycache__`). Snapshots never
capture directories outside the work tree.

## Security and limits

**Path traversal.** Every path of a record passes `checkout_safe` (`path.rs:230`): no `..`, no
`.`, no backslash, no `.git` or `.asc` alias. On Windows every name passes `windows_valid`
(`path.rs:153`); a name that cannot be represented fails the restore, it is not hidden, because a
partial install is worse than none. Every path must lie under one of the layout's snapshot roots:
a record cannot write `package.json`, `.npmrc` or source files.

**Symlink escape.** A symlink target in a record must:

- be relative (no leading `/`, no drive letter, no UNC prefix, no backslash);
- consist of a leading run of `..` components followed only by plain names (no `.`, no `..` after
  a name), so lexical and real resolution agree;
- stay within the work tree root after the leading `..` run is applied to the link's directory;
- not pass through another symlink entry of the snapshot in its plain-name part, except as its
  last component.

Workspace links (`../../packages/a`) pass; a link to `/etc` or to `../../../outside` does not. A
target into `.git` or `.asc` is refused. Restore never follows a link while writing: parents are
checked with `Verified::ensure_parent` (`safe_path.rs:134`), an existing link at a destination is
removed, never written through (`plain.rs:449`), and the temporary directory is created fresh.

**Case-insensitive file systems.** The key holds `os`, but a case-sensitive APFS volume or a
case-folding ext4 directory makes the producer's and the consumer's rules differ. Restore runs
`case_collisions` (`safe_path.rs:88`) over the record's paths on a volume that folds case and
fails on any collision, naming both paths; the fallback installs. Capture records nothing extra.

**Windows long paths.** Restore writes with verbatim `\\?\` paths (as `asc-vfs`'s `win::verbatim`
does), so the restore itself never fails on length. Node reads long paths through libuv, but other
tools may not. When the root's length plus the record's longest path exceeds 259 characters and
`LongPathsEnabled` is off, restore warns once and names the longest path.

**Decompression and size.** Objects are decoded within the existing decode cap. The record limits
are checked before any object is fetched. A restore checks free space on the target volume for a
copy restore (the logical size) before it starts.

**Concurrency.** Two restores of one root take a lock file `<root>.asc-restore.lock`; the second
waits up to 60 seconds, then fails with a message. A restore never removes a root it did not write
unless the marker says asc wrote it; a hand-made `node_modules` without a marker is moved to
`node_modules.asc-old-<time>` and removed only after the new root is in place.

## Implementation units

Each unit has tests and updates the docs it names. Acceptance runs from the workspace root.

1. **Key.** `crates/asc-wc/src/depsnap/mod.rs`, `crates/asc-wc/src/depsnap/key.rs`: `KeyParts`,
   `SnapshotKey`, file digests, config scrubbing, env name refusal. Tests: each part changes the
   key; file order does not; an `_authToken` line does not; `--explain` names the differing part.
   Docs: this file's key section stays the reference. `cargo test -q -j 4 -p asc-wc depsnap::key`
2. **Platform probes.** `crates/asc-wc/src/depsnap/platform.rs`: Node os and arch names, ELF
   `PT_INTERP` parse, glibc version string, `node` ABI probe, manager version from
   `packageManager`. Tests over ELF fixtures in `crates/asc-wc/tests/fixtures/elf/`.
   `cargo test -q -j 4 -p asc-wc depsnap::platform`
3. **Configuration and layout.** `crates/asc-wc/src/depsnap/config.rs` (`.asc/snapshots.toml`),
   `crates/asc-wc/src/depsnap/layout.rs` (manager detection, snapshot roots, host-bound files,
   default install argv). Tests per layout fixture (npm, pnpm, bun, yarn PnP, yarn node-modules).
   Docs: `docs/CONFIG.md`. `cargo test -q -j 4 -p asc-wc depsnap::layout`
4. **Capture.** `crates/asc-wc/src/depsnap/capture.rs`: walk, trees, git SHA-1s, junction targets,
   empty directories, special files, limits, relocatability scan and placeholder, scripts
   declared. Tests: hard links dedupe, absolute-root detection, refused FIFO, limit breaches.
   `cargo test -q -j 4 -p asc-wc depsnap::capture`
5. **Restore.** `crates/asc-wc/src/depsnap/restore.rs`, `crates/asc-wc/src/depsnap/links.rs`:
   temporary directory and rename, reflink, assembled copies, plain copy, link target rules,
   junctions, case collisions, verbatim paths, npm hidden lockfile mtime, marker, lock file,
   incremental restore by `leafdiff`. Tests include hostile trees (escaping links, `..` after a
   name, links through links, `.git` aliases, colliding names).
   `cargo test -q -j 4 -p asc-wc depsnap::restore`
6. **Record.** `crates/asc-ci/src/snapshot.rs`, `StoreKind::Snapshot` in
   `crates/asc-ci/src/cache.rs`,
   the `asc-deps-snapshot` namespace in `crates/asc-core/src/sshsig.rs`: `SnapshotRecord`, encode,
   decode, validate, sign and verify. Tests: trailing bytes, limits, signature replay from another
   namespace refused. `cargo test -q -j 4 -p asc-ci snapshot`
7. **Server store.** `crates/asc-serve/src/actions_store.rs`: the `snapshot` namespace, scope
   rules, retention, per-scope cap, `ActionsRoots` tree roots, write checks (objects present,
   lockfile and manifest digests at the changeset), purge. Tests: a pull request cannot write
   `trusted`; a record with a wrong lockfile digest is refused; an expired record stops rooting its
   tree. Docs: `docs/SPEC.md` §12.7. `cargo test -q -j 4 -p asc-serve actions_store::snapshot`
8. **Machine tier.** `crates/asc-wc/src/depsnap/local.rs`: `<asc home>/deps/index` and
   `assembled`, eviction under `asc.deps.cacheSize`, hook into `asc gc`
   (`crates/asc/src/cli/gc.rs`). Tests: eviction order, a non-relocatable record never leaves the
   machine. `cargo test -q -j 4 -p asc-wc depsnap::local`
9. **CLI.** `crates/asc/src/cli/deps/snapshot.rs` and registration in
   `crates/asc/src/cli/deps/mod.rs`: `key`, `restore`, `save`, `install`, `verify`, `ls`, `purge`,
   the exit statuses, the scratch home for `install`. Tests against the in-memory API (`MemApi` of
   `cache.rs`). Docs: `docs/CLI.md`, `docs/CHEATSHEET.md`.
   `cargo test -q -j 4 -p asc deps::snapshot`
10. **Verify.** `crates/asc-wc/src/depsnap/verify.rs`: package-lock v2 and v3 (`packages` map:
    each path's `package.json` has the locked version), pnpm (`.pnpm/<name>@<version>` present
    for every locked package), tree-against-key comparison. Pure and bounded like the server's
    parsers. Tests over fixtures. `cargo test -q -j 4 -p asc-wc depsnap::verify`
11. **Virtual mount.** `crates/asc-vfs/src/depmount.rs` and a `deps` field in `Options`
    (`crates/asc-vfs/src/core.rs:85`): project snapshot trees at snapshot roots, copy-up writes,
    hydration prefetch of `package.json`, `*.node` and `.bin`. Tests with `MemProvider`. Docs:
    `docs/WORKTREES.md`. `cargo test -q -j 4 -p asc-vfs depmount`
12. **CI.** `crates/asc-ci/src/checkout.rs` (`ASC_DEPS_KEY_<NAME>` export), job summary lines in
    `crates/asc/src/cli/deps/snapshot.rs`, the unreachable-server fallback. Tests: export
    escaping through `env.rs`, fallback on a refused connection. Docs: `docs/ci/`.
    `cargo test -q -j 4 -p asc-ci checkout::deps`
13. **Worktrees and SPEC.** `crates/asc/src/cli/worktree.rs`: restore after `NoReflink` or
    `LockfilesDiffer`. Tests: a worktree on a volume without reflink gets a restored root. Docs:
    `docs/WORKTREES.md` "Installed dependencies", `docs/SPEC.md` (remove `node_modules` snapshots
    from the out-of-scope list, add the section), `docs/ROADMAP-NEXT.md` row 22.
    `cargo test -q -j 4 -p asc worktree::deps`

Order: 1, 2, 3, 6, 4, 5, 7, 9, 12, 8, 10, 13, 11. Units 1 to 9 give a working CI path; 11 is the
largest and is last.

## Open questions

1. **Hash whole `package.json` files or only install-relevant fields?** Whole files miss on a
   change to `scripts` or `description`. Recommended: whole files in v1. Field selection is
   manager-specific and a wrong omission gives a wrong tree, which is worse than a miss.
2. **glibc version in the key, or only the family?** The version splits the cache between runner
   images. Recommended: keep the version. Producers and consumers that matter (CI runners) use few
   images, and an addon built against a newer glibc fails at load time, far from the cause.
3. **Can a developer's machine publish to a shared scope?** Recommended: no. Developers write
   `dev-<principal>` only, and normally consume `trusted`. Shared publication stays with protected
   branch CI.
4. **Are signatures required by default?** Recommended: no. The server's scope rules already
   protect its own storage. Require them (`asc.deps.requireSigned`) when snapshots come from storage
   other parties can write.
5. **Should the server verify layout against the lockfile on write?** Recommended: not in v1. It
   means reading every `package.json` of the tree on the server. Keep the cheap digest checks there
   and run `asc deps verify` in trusted CI; revisit when unit 10 has measurements.
6. **Share snapshots across repositories of one owner?** Recommended: no. Records and objects stay
   in the repository namespace, like Actions caches, so read access needs no new rule.
7. **Where do the lockfile parsers for `verify` live?** The server's parsers are in `asc-serve`,
   which the client does not always build. Recommended: unit 10 writes the two small parsers it
   needs in `asc-wc`; moving the server's pure parsers into a shared crate is a later refactor
   with its own unit.
8. **Two producers must agree before `trusted` accepts a `scripts = "none"` snapshot?**
   Recommended: offer it as `[deps.snapshots] require_agreement = 2`, off by default. It doubles
   install time on trunk and only helps when scripts do not run.
9. **Keep `.cache` and `.vite` across a full (non-incremental) restore?** Recommended: yes. Move
   them into the new root before the rename, so a key change does not make builds cold.
