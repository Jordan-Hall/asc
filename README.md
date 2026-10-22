# asc

`asc` is ASC's source control system. It is modelled on Anthropic's Scion (["Redesigning source control"](https://www.anthropic.com/research/redesigning-source-control)), and its commands mirror git's.

- **Changesets, not git objects.** Bonsai-style changesets and trees are content-addressed with BLAKE3 and stored as immutable objects. Git SHA-1s are deterministic derived data, so the git commits you have today keep their SHAs.
- **Storage anywhere.** The same engine runs over an S3-compatible bucket (S3, MinIO, R2, Garage, Ceph), local disk, or an `asc serve` server. It also works fully local-only, with no server.
- **Lazy working copies.** `asc clone`, `asc switch` and `asc worktree add` download metadata only. File contents are fetched the first time something reads them: ProjFS on Windows, FUSE on Linux, reflink copies elsewhere. All worktrees on a machine share one object cache.
- **git-shaped CLI.** `asc status`, `asc add`, `asc commit`, `asc switch`, `asc log`, `asc push` and the rest take git's flags and print git's output formats. A real `.git` projection means git, turbo, IDEs and husky keep working inside an asc working copy, and commits made with `git commit` are picked up.
- **API-first CI.** Diffs, merge bases, logs and blame run on the server. A CI plan job needs one RPC and no checkout. Turbo hashes a virtual checkout without reading any files, and the turbo remote cache lives in the same object store.
- **Forgejo, fully on asc.** A Forgejo fork reads and writes repositories through `asc serve`, and the git bridge becomes a cache. Cutover is staged and reversible: git-remote mode, then mirror mode, then asc as source of truth, then Forgejo reads through asc, then Forgejo writes through asc.

## Documents

| File | Contents |
|---|---|
| [`docs/SPEC.md`](docs/SPEC.md) | The authoritative design: data model and byte formats, storage, RPCs, server, daemon and VFS, CLI map, turbo and CI, Forgejo plan, migration, security, roadmap |
| [`docs/PLAN.md`](docs/PLAN.md) | Ordered implementation units, each with owned files, dependencies, an acceptance command and a brief |
| `docs/designs/*.md` | The three competing designs this spec was synthesised from (background only) |
| `docs/research/*.md` | Research on ASC's repo, Forgejo internals, prior art, the Rust crate stack, and turbo/CI |

## Layout (once the workspace is built)

```
crates/asc-core    data model, canonical codec, git rendering, content ingest
crates/asc-store   Objects/Refs traits; bucket, cache directory, SQL refs, bucket refs
crates/asc-engine  trees, commits, graph, derived data, diff, merge, land, blame, sync
crates/asc-git     index, projection, import/export, git-remote-asc, git-remote mode
crates/asc-rpc     gRPC protocol (proto/asc/v1), server/client, local IPC
crates/asc-wc      $ASC_HOME, state.db, workspaces, status, switch, plain mode
crates/asc-vfs     ProjFS and FUSE providers, submodule composition
crates/asc-serve   asc serve: refs, outbox, Forgejo hooks, mirror, land, turbo cache
crates/asc-ci      asc ci checkout/affected/changed/warm
crates/asc         the single binary (asc, asc daemon, asc serve, git-remote-asc, asc-fsmonitor)
```

## Status

The design is complete and implementation has not started. Begin with PLAN unit U01. Units U01–U25 produce a working local-only `asc` (init, add, commit, status, diff, log, branch, switch) before anything touches the network or a virtual filesystem.

Toolchain: Rust 1.95 (edition 2024), git ≥ 2.40, and Docker for the S3 and Postgres integration tests. On Windows, the virtual filesystem needs the `Client-ProjFS` optional feature.
