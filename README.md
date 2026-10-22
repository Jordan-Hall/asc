# asc

`asc` is ASC's source control system. It is modelled on Anthropic's Scion (["Redesigning source control"](https://www.anthropic.com/research/redesigning-source-control)), and its commands mirror git's.

- **Changesets, not git objects.** Bonsai-style changesets and trees are content-addressed with BLAKE3 and stored as immutable objects. Git SHA-1s are deterministic derived data, so the git commits you have today keep their SHAs.
- **Storage anywhere.** The same engine runs over an S3-compatible bucket (S3, MinIO, R2, Garage, Ceph), local disk, or an `asc serve` server. It also works fully local-only, with no server.
- **Lazy working copies.** `asc clone`, `asc switch` and `asc worktree add` download metadata only. File contents are fetched the first time something reads them: ProjFS on Windows, FUSE on Linux, reflink copies elsewhere. All worktrees on a machine share one object cache.
- **git-identical CLI, installable as `git`.** `asc status`, `asc add`, `asc commit`, `asc switch`, `asc log`, `asc push` and the rest take git's flags and print git's output, messages and exit codes. Installed as `git`, asc serves every git command inside an asc working copy and falls through to real git everywhere else (including `git clone`/`git init` of a separate repository), so scripts, hooks (husky, lint-staged) and agents such as Claude Code keep working. Every hook, alias and editor that asc starts finds asc first on `PATH`, even when the shim is not installed for interactive use. Each working copy has a minimal synthetic `.git` for Turborepo only; IDE git panels are replaced by ASC's plugins, which use asc's API. The default branch is `master`.
- **Built-in project graph and `asc affected`.** asc detects Rust (Cargo), Python (uv, pyproject, requirements), .NET (sln, csproj) and Bun/Node workspaces itself, as derived data per changeset, and computes affected projects (lockfile-aware, with per-project input hashes) locally or on the server. Output formats include JSON, a turbo filter, an nx project list and an Actions matrix.
- **API-first CI, Scion-style efficiency.** Diffs, merge bases, logs, blame and `Affected` run on the server. A CI plan job needs one RPC and no checkout. Fetches are want-only and resumable, clients keep a bounded history window and ask the server for older history, every cache tier speaks the same RPCs (`asc serve --role cache`), seed snapshots warm cold machines, and a benchmark gate holds asc faster than git. Turbo hashes a virtual checkout without reading any files, and the turbo remote cache, Actions caches and artifacts live in the same object store. ASC's Actions fork (`asc/actions`) makes CI use asc with no workflow edits.
- **Stacked changes and stacked PRs.** New verbs (`asc create`, `modify`, `absorb`, `restack`, `sync`, `submit`, `sl`, `undo`, …) give Graphite/Sapling/jj-style stacks with stable change ids and an operation log, without changing any git command or git SHA. The Forgejo fork shows stacked PRs natively (each PR's base is its parent, becoming the trunk when the parent merges) and lands stacks atomically through a merge-when-ready queue. Blame, history, commit-graph and worktree data that GitLens/GitKraken users rely on are served through asc's API for ASC's plugins.
- **Forgejo, fully on asc.** A Forgejo fork reads and writes repositories through `asc serve`, and the git bridge becomes a cache. Cutover is staged and reversible: git-remote mode, then mirror mode, then asc as source of truth, then Forgejo reads through asc, then Forgejo writes through asc.

## Documents

| File | Contents |
|---|---|
| [`docs/SPEC.md`](docs/SPEC.md) | The authoritative design: data model and byte formats, storage, RPCs, server, daemon and VFS, CLI map and the `git` shim, turbo and CI, project graph and `affected`, Forgejo plan, migration, security, operations, testing, roadmap |
| [`docs/PLAN.md`](docs/PLAN.md) | Ordered implementation units, each with owned files, dependencies, an acceptance command and a brief |
| [`docs/CARRY.md`](docs/CARRY.md) | Obligations that built units created for later units; read before every unit |
| `docs/designs/*.md` | The three competing designs this spec was synthesised from (background only) |
| `docs/research/*.md` | Research on ASC's repo, Forgejo internals, prior art, the Rust crate stack, turbo/CI, the Scion article, `stacks.md` (the stacked-changes design folded into SPEC §20), and `scion-checklist.md` (the required coverage list, including the user requirements) |

## Layout (once the workspace is built)

```
crates/asc-core    data model, canonical codec, git rendering, content ingest
crates/asc-store   Objects/Refs traits; bucket, cache directory, SQL refs, bucket refs
crates/asc-engine  trees, commits, graph, skip lists, derived data, diff, merge, land, blame, sync, projects
crates/asc-graph   project graph: detectors (Rust, Python, .NET, Node), lockfiles, ownership, affected
crates/asc-state   $ASC_HOME, state.db, staging
crates/asc-git     RealGit, synthetic .git for turbo, import/export, git-remote mode
crates/asc-rpc     gRPC protocol (proto/asc/v1), server/client, retries and limits, local IPC
crates/asc-wc      workspaces, status, switch, plain mode, worktrees
crates/asc-vfs     ProjFS and FUSE providers, submodule composition
crates/asc-serve   asc serve: refs, outbox, Forgejo hooks, mirror, land, indexes, cache role, seeds, Projects, turbo cache
crates/asc-ci      asc ci checkout/affected/changed/warm/cache/artifact
crates/asc-sim     hermetic simulation (tests)
crates/asc         the single binary (asc, asc daemon, asc serve, and the git shim)
```

## Status

The design is at SPEC v1.3 (audit round 3; stacked changes folded in as SPEC §20). Units U01–U12 and U14 are built and U15 is in progress (`docs/CARRY.md` lists the obligations they created). Next is PLAN unit U01b, the v1.1–v1.3 skeleton delta. Units up to U25 produce a working local-only `asc` (init, add, commit, status, diff, log, branch, switch), installable as `git` and checked by the git compatibility suite, before anything touches the network or a virtual filesystem. The project-graph units G01–G08 deliver `asc affected` locally in the same phase. The stack track (C-units, the S-track of `stacks.md` renamed) adds local stacks after the merge/rebase units (C01–C15), stack sync and submit with the server (C17–C32), and stacked PRs in the Forgejo fork after K17 (C34–C48).

Toolchain: Rust 1.95 (edition 2024), git ≥ 2.40, and Docker for the S3 and Postgres integration tests. On Windows, the virtual filesystem needs the `Client-ProjFS` optional feature.
