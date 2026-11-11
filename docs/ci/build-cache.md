# Rust builds in CI with the asc build cache

`actions/asc/rust-cache` points cargo at the asc build cache (`RUSTC_WRAPPER=asc`). It replaces
caches that save `target/` and `~/.cargo` as one tarball per key.

## Why backups stay small

A tarball cache writes a new archive of several gigabytes whenever its key changes, often on every
run. File-level backup tools see a changed file, so they copy each archive in full. They also copy
the runner's cache volume again after every job.

The asc build cache stores each compile once, under a key made from the compile's inputs:

| Part | Where | Written | What a backup sees |
|---|---|---|---|
| Blobs (`cas/`) | the job's own store, then the server | once, read only, named by their BLAKE3 hash; zstd on a volume that cannot reflink (`asc.buildcache.compress`) | only new compiles; a blob never changes after it is written |
| Entries (`ac/`) | the same | once per compile, a few hundred bytes | small new files |
| The job's own store | `RUNNER_TEMP/asc-buildcache` | during the job | nothing: the runner deletes it when the job ends |
| Host stores (`lower`) | a directory the runner mounts read only | never by a job | only what the process that fills it adds |
| Server artefacts | `asc serve`, under `/v8/artifacts/asc-<key>` | once per new compile, by trusted jobs | new objects only |

A compile whose inputs did not change is a hit, so nothing new is written. A changed crate writes
its own compiles and nothing else. `CARGO_INCREMENTAL=0` is set because incremental data is specific
to one machine and changes on every build. It would only add churn.

A lookup goes through these tiers in order:

1. The job's own store.
2. Each read-only host store, in order. A hit there is copied into the job's store. The host
   store's files are not written and their times are not touched, so a backup of the host sees no
   change.
3. The server, unless the tier is `off`.

Every blob is verified against its hash, whichever tier it comes from. A damaged blob, a missing
blob, or an entry with a path that leaves the target directory is a miss.

## Using it

```yaml
- uses: actions/checkout@v4
- uses: asc/actions/asc/setup@v1          # or any step that puts asc on PATH
  with:
    version: ${{ vars.ASC_VERSION }}
    url: ${{ vars.ASC_URL }}
    sha256: ${{ vars.ASC_SHA256 }}
- uses: asc/actions/asc/rust-cache@v1
- run: cargo test --workspace
- if: ${{ always() }}
  uses: asc/actions/asc/rust-cache/save@v1
```

| Input | Default | Meaning |
|---|---|---|
| `server` | the job's `ASC_REMOTE`, else the forge | The asc repository URL of the cache server. Must be `https`. |
| `token` | the task token | Exported as `ASC_TOKEN` when the job has none. |
| `lower` | the job's `ASC_HOST_BUILDCACHE` | Read-only host stores, separated by `:` (`;` on Windows). |
| `mode` | `read` on pull requests, else `read-write` | The server tier: `off`, `read` or `read-write`. |
| `dir` | `RUNNER_TEMP/asc-buildcache` | The job's own store. |

The action writes no files outside `dir`. It sets these variables for the following steps:
`RUSTC_WRAPPER`, `CARGO_INCREMENTAL=0`, and `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_n` and
`GIT_CONFIG_VALUE_n` for `asc.buildcache.dir`, `lower`, `server` and `remote`. It adds its entries
after any that the job set already. If asc is not on `PATH`, the action prints a warning, sets
nothing, and the job builds without a cache.

The asc wrapper sends new compiles in the background. A job can end before those uploads finish, so
`asc/rust-cache/save` runs `asc cache push --rev <commit>` as the last step. That step does nothing
when the cache is off or when the server tier is not `read-write`.

### Pull requests

A pull request job reads the server and never writes to it. The job's own store is deleted when the
job ends. Host stores are mounted read only. So a pull request cannot put a compile where another job
will read it. Release builds use no build cache: each release binary is compiled from source.

The token is exported as `ASC_TOKEN`, which the job's later steps can read. That includes cargo's
build scripts and tests. They run in the same process tree as the wrapper, so they could read any
file or variable given to it. A token file would not hide it from them. The token is the job's own
task token, and checkout already gives the job the same token. If your forge can issue a narrower
token for pull request jobs, pass it as `token`.

## Runner and host setup

These rules apply to any runner that runs jobs in containers or in its own work directory:

- **Keep the runner's work and cache directories out of file backups.** These include the job work
  directories, the action cache server's directory, and the container volumes of jobs. Everything
  in them can be made again. The asc server's store, which the server's own backup covers, is what
  needs a backup ([`../runbooks/backup.md`](../runbooks/backup.md)).
- **Turn off the runner's tarball cache for Rust jobs** after moving to `asc/rust-cache`. Delete its
  old archives once, so the next backup does not copy them again.
- **Optional host store.** A process outside the jobs fills a directory, for example
  `/var/cache/asc/buildcache`. It can run `asc cache warm master` in a clone, as the user that owns
  the directory, with `asc.buildcache.dir` set to that directory. The runner mounts the directory
  into jobs read only and sets `ASC_HOST_BUILDCACHE` to it. See
  [`../../deploy/runner/runner-config-snippet.yaml`](../../deploy/runner/runner-config-snippet.yaml).
- `asc.buildcache.dir`, `asc.buildcache.lower`, `asc.buildcache.server` and
  `asc.buildcache.remote` are ignored in a repository config that the current user does not own.
  So a cloned repository cannot point the cache at a directory or server of its choice. The action
  sets them through the environment, which is trusted the same way as the command line.

## Checking it

```sh
asc cache status          # store, lower stores (read only), server tier, hit rate per tool
asc cache status --json   # the same, with "lower": [...]
```
