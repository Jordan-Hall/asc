<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/brand/wordmark-dark.svg">
    <img src="docs/brand/wordmark-light.svg" alt="ASC" height="48">
  </picture>
</p>

<p align="center"><strong>Source control for people and their agents.</strong></p>

# asc

`asc` is ASC, Agent Source Control: source control for people and their agents. It is modelled on Anthropic's Scion (["Redesigning source control"](https://www.anthropic.com/research/redesigning-source-control)), and its commands mirror git's.

- **Changesets, not git objects.** Bonsai-style changesets and trees are content-addressed with BLAKE3 and stored as immutable objects. Git SHA-1s are deterministic derived data, so the git commits you have today keep their SHAs.
- **Storage anywhere.** The same engine runs over an S3-compatible bucket (S3, MinIO, R2, Garage, Ceph), local disk, or an `asc serve` server. It also works fully local-only, with no server.
- **Lazy working copies.** `asc clone`, `asc switch` and `asc worktree add` download metadata only. File contents are fetched the first time something reads them: ProjFS on Windows, FUSE on Linux, reflink copies elsewhere. All worktrees on a machine share one object cache.
- **git-identical CLI, installable as `git`.** `asc status`, `asc add`, `asc commit`, `asc switch`, `asc log`, `asc push` and the rest take git's flags and print git's output, messages and exit codes. Installed as `git`, asc serves every git command inside an asc working copy and falls through to real git everywhere else (including `git clone`/`git init` of a separate repository), so scripts, hooks (husky, lint-staged) and agents such as Claude Code keep working. Every hook, alias and editor that asc starts finds asc first on `PATH`, even when the shim is not installed for interactive use. Each working copy has a minimal synthetic `.git` for Turborepo only; IDE git panels are replaced by ASC's plugins, which use asc's API. The default branch is `master`.
- **Built-in project graph and `asc affected`.** asc detects Rust (Cargo), Python (uv, pyproject, requirements), .NET (sln, csproj) and Bun/Node workspaces itself, as derived data per changeset, and computes affected projects (lockfile-aware, with per-project input hashes) locally or on the server. Output formats include JSON, a turbo filter, an nx project list and an Actions matrix.
- **API-first CI, Scion-style efficiency.** Diffs, merge bases, logs, blame and `Affected` run on the server. A CI plan job needs one RPC and no checkout. Fetches are want-only and resumable, clients keep a bounded history window and ask the server for older history, every cache tier speaks the same RPCs (`asc serve --role cache`), seed snapshots warm cold machines, and a benchmark gate holds asc faster than git. Turbo hashes a virtual checkout without reading any files, and the turbo remote cache, Actions caches and artifacts live in the same object store. ASC's Actions fork (`asc/actions`) makes CI use asc with no workflow edits.
- **Stacked changes and stacked PRs.** New verbs (`asc create`, `modify`, `absorb`, `restack`, `sync`, `submit`, `sl`, `undo`, …) give Graphite/Sapling/jj-style stacks with stable change ids and an operation log, without changing any git command or git SHA. The Forgejo fork shows stacked PRs natively (each PR's base is its parent, becoming the trunk when the parent merges) and lands stacks atomically through a merge-when-ready queue. Blame, history, commit-graph and worktree data that GitLens/GitKraken users rely on are served through asc's API for ASC's plugins.
- **Forgejo, fully on asc (staged; see docs/STATUS.md).** ASC Hub, a Forgejo fork, reads and writes repositories through `asc serve`, and the git bridge becomes a cache. Cutover is staged and reversible: git-remote mode, then mirror mode, then asc as source of truth, then Forgejo reads through asc, then Forgejo writes through asc.

## Install

Releases are built for Linux (x86_64, aarch64), macOS (x86_64, aarch64) and Windows (x86_64) by `.github/workflows/release.yml`, attached to the GitHub release, and mirrored to whatever host serves your downloads (`scripts/release/publish-to-hub.sh`). The installers read that host from `ASC_DOWNLOAD_BASE_URL`: the directory that holds `latest/version` and `<version>/`. There is no built-in host name; use the one your ASC Hub (or any static file host) serves. Full detail, verification and the layout: [`docs/RELEASE.md`](docs/RELEASE.md).

```sh
# Linux and macOS: installs to ~/.local/share/asc-install and links ~/.local/bin/asc
export ASC_DOWNLOAD_BASE_URL=https://downloads.example.org/asc
sh scripts/install.sh            # --dry-run shows the plan; --version X.Y.Z pins a version
```

```powershell
# Windows: installs to %LOCALAPPDATA%\Programs\asc and puts its current directory on the user PATH
$env:ASC_DOWNLOAD_BASE_URL = 'https://downloads.example.org/asc'
./scripts/install.ps1            # -DryRun shows the plan; -Version X.Y.Z pins a version
```

- **Integrity and trust.** The installers verify the archive against `SHA256SUMS`. Set `ASC_RELEASE_PUBKEY` (the minisign public key of your release) to also require a valid signature on `SHA256SUMS`; without it the checksum proves only that the download matches what the host published. Plain `http` is refused except on the loopback interface with `ASC_ALLOW_LOOPBACK_HTTP=1`.
- **Package managers.** Each release carries a Homebrew formula (`asc.rb`), a Scoop manifest (`asc.scoop.json`) and winget manifests (`winget-asc.asc*.yaml`), rendered from `packaging/`.
- **Windows MSI and Linux service.** The MSI and the systemd unit of the Forgejo release flow are described in [`docs/RELEASING.md`](docs/RELEASING.md).
- **Editors.** `asc-vscode-<ver>.vsix` and `asc-jetbrains-<ver>.zip` are attached to each release.
- **Update:** `asc self-update` (also `asc upgrade`; signature-checked, side by side, same source variable). It updates installs made by `install.sh`, `install.ps1` or the MSI; for Homebrew, Scoop or winget installs use that tool.
- **Version:** `asc --version` prints the version, the git it found and the commit it was built from.
- **From source:** Rust 1.95, `cargo build --release -p asc`. Default features are `serve`, `vfs-projfs`, `vfs-fuse`, `git-compat`, `git-ssh` and `http3`.

Real git 2.40 or later must be installed. Run `asc doctor` afterwards. Step by step: [`docs/GETTING-STARTED.md`](docs/GETTING-STARTED.md).

## Quick start

```sh
# a local-only repository, no server
asc init demo && cd demo
echo hi > a.txt && asc add a.txt && asc commit -m "first"

# or clone from the staging server (see docs/GETTING-STARTED.md for the token)
asc clone https://asc-staging.example.org/<owner>/<repo>
asc create -m "my change"        # start a change (a stack of them is allowed)
asc submit -s                    # send the stack up as draft changes
asc sync                         # fetch, fast-forward trunk, drop landed changes, restack
```

## Server components

| Component | What it is | Where it is documented |
|---|---|---|
| `asc` | The client and the one binary: CLI, daemon, `git` shim and `asc serve`. | this repository |
| `asc serve` | The storage and RPC server (gRPC, HTTP/2, optional HTTP/3), plus smart HTTP and SSH for plain `git`. | [`deploy/`](deploy/README.md), [`deploy/coolify/`](deploy/coolify/README.md) |
| ASC Hub | The ASC fork of Forgejo v16 (repository `asc/asc-hub`, image `forge.example.org/asc/asc-hub`): web UI, review, Actions, packages. It asks `asc serve` for repository data. | `docs/asc/README.md` in the ASC Hub repository |

See [`docs/STATUS.md`](docs/STATUS.md) for the architecture sketch and what runs where.

## Documents

| File | Contents |
|---|---|
| [`docs/GETTING-STARTED.md`](docs/GETTING-STARTED.md) | Install, authenticate, clone, the daily loop, plain git against `asc serve` |
| [`docs/STATUS.md`](docs/STATUS.md) | What is built, what runs where, what is missing |
| [`docs/ONBOARDING.md`](docs/ONBOARDING.md) | The `git` shim, editor settings, `asc doctor` findings |
| [`docs/CHEATSHEET.md`](docs/CHEATSHEET.md) | git commands and what asc does differently |
| [`docs/RELEASING.md`](docs/RELEASING.md) | How a release is built, signed and published |
| [`docs/SPEC.md`](docs/SPEC.md) | The authoritative design (normative) |
| [`docs/PLAN.md`](docs/PLAN.md) | Ordered implementation units with acceptance commands (normative) |
| [`docs/CARRY.md`](docs/CARRY.md) | Obligations that built units created for later units |
| [`docs/divergences.md`](docs/divergences.md), [`docs/adapter-gaps.md`](docs/adapter-gaps.md) | Where asc differs from pinned git; harness gaps |
| [`docs/git-fidelity.md`](docs/git-fidelity.md), [`docs/git-quirks.md`](docs/git-quirks.md), [`docs/git-defects.md`](docs/git-defects.md) | The git fidelity method and the quirk decisions |
| [`docs/api.md`](docs/api.md), `docs/plugin-api*.md` | The RPC/API surface and the IDE plugin API |
| [`docs/runbooks/`](docs/runbooks) | Per-tier operations runbooks |
| `docs/designs/*.md`, `docs/research/*.md` | Background: competing designs and research |

## Layout

```
crates/asc           the single binary (asc, asc daemon, asc serve, the git shim, the api bridge)
crates/asc-core      data model, canonical codec, git rendering, content ingest
crates/asc-store     Objects/Refs traits; bucket, cache directory, SQL refs, bucket refs
crates/asc-engine    trees, commits, graph, derived data, diff, merge, land, blame, sync, projects
crates/asc-graph     project graph: detectors (Rust, Python, .NET, Node), lockfiles, affected
crates/asc-state     $ASC_HOME, state.db, staging
crates/asc-git       RealGit, synthetic .git for turbo, import/export, git compatibility views
crates/asc-rpc       gRPC protocol (proto/asc/v1), server/client, retries and limits, h2 and h3
crates/asc-wc        workspaces, status, switch, plain mode, worktrees
crates/asc-vfs       ProjFS and FUSE providers, submodule composition
crates/asc-serve     asc serve: refs, land, smart HTTP, SSH, indexes, cache role, Projects, turbo cache
crates/asc-ci        asc ci checkout/affected/changed/warm/cache/artifact
crates/asc-codeintel code intelligence: doc model, chunking, secret scanner
crates/asc-embed     embedding runtime for code intelligence (server only; asc.exe never links it)
crates/asc-mcp       MCP protocol layer (skeleton; the MCP track is in progress)
crates/asc-sim       hermetic simulation (tests)
```

## Status

See [`docs/STATUS.md`](docs/STATUS.md). In short: the local client, stacks, `asc serve`, the git transports and the release pipeline exist and a standalone staging server runs; IDE plugins are in progress; the Forgejo cutover stages are not done.

Toolchain: Rust 1.95 (edition 2024), git 2.40 or later, and Docker for the S3 and Postgres integration tests. On Windows, the virtual filesystem needs the `Client-ProjFS` optional feature.
