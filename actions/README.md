# ASC Actions

The `asc/actions` set (SPEC 12.7, PLAN U64): forks of `checkout`, `cache`, `upload-artifact` and
`download-artifact` that call `asc ci ...` instead of git, plus `asc/setup`, `asc/changed`,
`asc/affected` and `asc/turbo-cache`. This directory is the source of the `asc/actions` mirror.

Run the self test with one command:

    bash actions/test/selftest.sh            # offline, starts nothing
    bash actions/test/selftest.sh --mirror https://git.example.com/asc/actions/raw/branch/master
    ASC_SERVER_URL=... ASC_TOKEN=... ASC_BIN=/path/to/asc bash actions/test/selftest.sh --live

`--live` runs `asc ci checkout` in a container with a read-only host cache mount against an `asc
serve` you started, and asserts the synthetic `.git`, `TURBO_SCM_BASE` and `TURBO_API`. The script
never starts a server. It needs Python with PyYAML for the static checks.

## Actions

| Action | Calls | Notes |
| --- | --- | --- |
| `checkout` | `asc ci checkout` | Keeps `ref`, `fetch-depth` (ignored), `path`, `submodules` (composition, not fetched), `lfs` (ignored), `sparse-checkout` (cones). Falls back to a plain `git fetch` when the server has no `/.well-known/asc` or `asc` is absent. |
| `cache` | `asc ci cache restore` | Output `cache-hit`. Composite actions have no post step, so save with `cache/save`; the runner fork saves in its post phase. |
| `upload-artifact`, `download-artifact` | `asc ci artifact` | Stored as objects plus a manifest in `/actions/v1`; downloads work across runs. |
| `asc/setup` | – | Downloads a release tarball over https, verifies its SHA-256, caches it by version in `RUNNER_TOOL_CACHE`. |
| `asc/changed`, `asc/affected` | `asc ci changed`, `asc ci affected` | No checkout. Outputs `all`, `count`, `matrix`, `turbo_filter`, `rust` and the switches of `.asc/ci.toml`. |
| `asc/turbo-cache` | – | Exports `TURBO_API`, `TURBO_TOKEN`, `TURBO_TEAM`, `TURBO_CACHE` (`remote:r` on pull requests). |

## Security rules

- Inputs reach scripts only through `env`, never by `${{ }}` inside `run` (checked by the self test).
- Tokens travel in the environment (`ASC_TOKEN`), never argv. The git fallback passes its header through
  git's environment config.
- Servers must be `https`; plain `http` is accepted only for loopback.
- `asc/setup` refuses anything without a lowercase 64 digit SHA-256 and verifies it before extracting.
- Repository, ref and path inputs reject newlines and `..`.

## Related files

- `deploy/runner/`: warm service and timer, and the runner config with the read-only cache mount.
- `docs/ci/asc-workflows.patch`: proposed changes to ASC's workflows (plan job, no `fetch-depth: 0`,
  no `submodules: recursive`, `green/master` post-merge job, deploy from `green/*`). Apply in
  `monorepo` with `git apply docs/ci/asc-workflows.patch`; ASC lands it as a PR.
- `docs/ci/asc-projects.toml`: proposed `.asc/projects.toml` (tags `image` and `deploy`, CI workflow
  files as global inputs).
- `deploy/forgejo`: set `[actions] DEFAULT_ACTIONS_URL` to the mirror (A01).
