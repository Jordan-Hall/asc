# Status

Honest state as of 2026-10-24 (branch `master`; `asc/v16` is the ASC Hub fork branch). Items marked (unverified) were not checked
against a running system when this page was written. SPEC and PLAN are the design; this page is
what exists.

## Built (in the repository, with tests)

- The local client: init, add, commit, status, diff, log, branch, switch, merge, rebase, stash,
  worktrees, blame, the `git` shim and `asc doctor`, checked against a pinned git (v2.55.0) by the
  compat suite (`docs/git-fidelity.md`).
- Stacks and branchless mode: `create`, `modify`, `absorb`, `restack`, `sync`, `submit`, `sl`,
  `undo`/`redo`/`op`, `stack get`, `fold`, `split` and more (`crates/asc/src/stack/cli/`). `asc submit`
  sends a stack as one `UpdateRefs` of per-change AGit refs.
- The project graph and `asc affected`; code-intelligence crates (`asc-codeintel`, `asc-embed`).
- `asc serve`: refs, outbox, land, token-store and Forgejo auth, cache role, smart HTTP, SSH
  (feature `git-ssh`, default on, off in config), HTTP/3 (feature `http3`, needs `[tls]`).
- Release pipeline: split Linux and Windows chains, signed write-once artefacts, MSI, winget
  manifests, `install.sh`, `asc upgrade` (`docs/RELEASING.md`).
- Bring-your-own storage (`docs/BYOS.md`): per-owner S3 targets with an SSRF-guarded, address-pinned client, a verification
  handshake, credentials sealed in the vault (XChaCha20-Poly1305, master key file, rotation), routing of a repository's objects
  to its target, resumable migration (copy, verify, switch, optional delete), health checks and quotas. Tested against an
  in-process S3 (`crates/asc-serve/src/storage/`); not yet run against a real provider or Postgres (unverified).
- `asc api` for the IDE plugins; the `crates/asc/src/bridge/` frame bridge is committed (its
  `Bridge.Run` decodes `OpResult` from `cli.proto`; not yet run against a live host, unverified).

## What runs where

| Thing | Where | How |
|---|---|---|
| asc staging server | `https://asc-staging.example.org`, Coolify (`apps.example.org`) | `deploy/coolify/asc-serve.standalone.compose.yml`: token-store auth, SQLite on a state volume (one replica), Garage S3 bucket `asc-staging`, no Forgejo integration, SSH git on host port 22223 (configured, not yet deployed or verified) |
| asc-serve image | `forge.example.org/asc/asc-serve:<sha12>` (also `:master`, `:<version>`) | built by `.forgejo/workflows/image.yml` on the `asc-build-trusted` runner for `master` pushes and `v*` tags |
| ASC Hub staging | `https://forge-staging.example.org` | image `forge.example.org/asc/asc-hub:<sha12>`, SQLite, own volume (ASC Hub repo, `docs/asc/STAGING.md`); `deploy/coolify/forgejo-staging.compose.yml` here |
| Full CI | nightly, on demand and on pull requests (`.forgejo/workflows/asc-ci.yml`) | pushes only build the image; a central gate verifies HEAD |
| Releases | Forgejo generic packages `asc/generic/asc` | `.forgejo/workflows/release.yml` or `build/release/local-release.sh` |

## Architecture

```
 developer machine                         Coolify / Forgejo host
+--------------------+  https (h2/h3)   +------------------------------+      +---------------+
| asc CLI + git shim  |----------------->| asc serve                     |----->| Garage S3     |
| asc daemon (VFS)    |  gRPC, discovery |  RPC, land, smart HTTP, SSH  |      | (objects)     |
| IDE plugins (api)  |                  |  refs/state: SQLite (staging)|      +---------------+
+--------------------+                  |  or Postgres (Forgejo mode)  |
| plain git ---------|--https/ssh------>+--------------^---------------+
+--------------------+                                  | RPC: repo data, authz
                                          +-------------+--------------+
 browser ---------------------------------| ASC Hub (Forgejo v16 fork)  |
                                          | web UI, review, Actions,   |
                                          | package registry           |
                                          +----------------------------+
```

`asc serve` owns repository data. ASC Hub (`asc/asc-hub`, a rebased Forgejo v16 fork; image
`forge.example.org/asc/asc-hub`) is the web side and asks `asc serve` over RPC. The
staging `asc serve` is not yet wired to ASC Hub.

## Not done

- IDE plugins (VS Code, IntelliJ): in progress; not released.
- Windows CI runner: not registered, so the Windows release chain and the Windows leg of full CI
  wait for it (`docs/RELEASING.md`, "Registering the Windows host runner"). Linux only ships.
- Releases for macOS and arm64 Linux: not built (jobs defined, manual only, no runner).
- Forgejo cutover stages (mirror, asc as source of truth, reads and writes through asc): staged
  design; the staging server runs standalone. Repository registration for `asc/soma` and
  `asc/dds` and the `/.well-known/asc` proxy route are not done (`deploy/coolify/README.md`).
- SSH on staging: enabled in the compose file (`[ssh]`, host key in the state volume, port `22223:2222`); not deployed yet, so unverified end to end.
- `asc doctor --store` is implemented (`crates/asc/src/cli/doctor.rs`: store transport checks and an
  integrity sample of stored objects, plus the create-exclusive probe). Its result against the
  staging bucket is not recorded here (unverified).
- Not built yet: MCP tools (`asc mcp` is a stub; `crates/asc-mcp` is a protocol skeleton), `asc cloud`,
  `asc live`, the `UploadScip` RPC (TODO(I16) in `crates/asc/src/cli/codeintel/scip.rs`) and the
  Semantic embedder.
- PLAN unit completion is tracked in `docs/PLAN.md` and `docs/CARRY.md`, not here.

## Known gaps in git compatibility

Full text in [`divergences.md`](divergences.md) (7 upstream decisions and 7 own divergences) and
[`adapter-gaps.md`](adapter-gaps.md) (6 harness gaps, all closed).

- D-5, D-6: in branchless mode pushing an unshared tag is rejected and `push --all` does not
  submit stacks. Status in the file: pending B12 (server policy).
- D-7: a hook installed through `rev-parse --absolute-git-dir` is not found; open, closes with U54w.
- D-1..D-3: hook message/index paths and `core.*` keys differ because of the synthetic gitdir.
- D-4: a refused credential is asked for once more. Q-6, Q-7 and others: see the file.
- IDE built-in git panels, LFS, `git submodule update` and packfile maintenance are not supported
  by design.
