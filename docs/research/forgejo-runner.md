# Forgejo runner fork for ASC CI

Status: research and design. Nothing in the fork is implemented yet.
Fork: `D:\dev\forgejo-runner`, branch `asc`, made from tag `v13.2.0`.
Related: `turbo-ci.md` sections 3 and 4 (the ASC CI analysis and the asc CI flow), `forgejo.md` (server seams).

---

## 0. Summary

- **Base version.** `v13.2.0` (2026-09-18) is the newest runner release. Forgejo server v16.0.6 is newer than runner v13.0.0. The runner and server talk through `code.forgejo.org/forgejo/actions-proto` (connect RPC), which is backward compatible. Neither the runner release notes nor the v16 admin documentation give a version matrix, and no v13 release note names a minimum server version. The v13 breaking changes are on the workflow side: removed `set-output`, `set-env` and `add-path`; expression errors are now hard errors; Docker 25.0 or later is required; `GITEA_*` runner variables are removed; `container.network_mode` is removed. Check ASC workflows for these before rollout.
- **What the fork adds.** Everything is opt-in, under one new top-level `asc:` config section, and works with plain git today:
  1. a Turborepo remote cache (`/v8` API) built into the runner, with per-job tokens and PR scopes;
  2. host toolchain caches (bun, cargo, uv, pip, npm, nuget, sccache) mounted into job containers;
  3. a host git mirror for each repository, exposed to `actions/checkout` through `GIT_ALTERNATE_OBJECT_DIRECTORIES`, with LFS objects included;
  4. a checkout-provider chain with an `asc` provider stub and an asc daemon socket mount.
- **What the fork does not add**, because the runner already does it (section 2): the actions cache server (`ACTIONS_CACHE_URL`, with PR write isolation), the action repository cache (shallow fetch of 1 commit since v13.2.0, #1722), Docker socket mounting, and plain bind mounts through `container.options` plus `valid_volumes`.
- **Main seam.** One small patch to `act/runner` lets the runner add bind mounts to the job container (and rewrite env paths for host jobs). Everything else lives in new packages under `internal/`. Rebases then touch about 4 upstream files.

---

## 1. Architecture map (v13.2.0)

### 1.1 Layout

| Path | Role |
|---|---|
| `main.go` | Calls `cmd.Execute(ctx)` with a SIGINT/SIGTERM context. |
| `internal/app/cmd` | Cobra commands: `daemon`, `one-job`, `register`, `create-runner-file`, `exec` (local `act` run), `validate`, `generate-config`, `cache-server`. `daemon.go` loads config, starts the cache (`run.SetupCache`), creates one client and one runner per `server.connections` entry (`createRunner`, a package var so tests can mock it), and starts the poller. `job.go` (`one-job`) does the same for a single task. |
| `internal/app/poll` | Poller: `FetchTask` loop with `runner.capacity` slots. Hands each task to `run.Runner.Run`. |
| `internal/app/run` | `runner.go`: turns one `runnerv1.Task` into an `act` run. Builds the env, registers the run with the cache proxy, fills `act/runner.Config`, plans the job and runs the executor. **This is where the fork injects per-job env and mounts.** |
| `internal/pkg/config` | Config model. Typed `Config` structs, plus `serialized*Settings` structs with `yaml` tags and an `applyTo(*Config) error` method each. Defaults in `config.New()`. `config.example.yaml` is embedded (`embed.go`) and printed by `generate-config`. |
| `internal/pkg/client` | Connect-RPC client to Forgejo (`FetchTask`, `UpdateTask`, `UpdateLog`, `Declare`). `backward.go` reads `forgejo_*` or `gitea_*` context keys. |
| `internal/pkg/report` | Log and state reporter; `mask.go` masks secrets in logs. |
| `internal/pkg/labels` | Label parsing: `name:docker://image`, `name:host`, `name:lxc://...`, or `name:<plugin>://arg`. |
| `act/` | The `act` fork, **vendored in-tree** as packages of the same module (`code.forgejo.org/forgejo/runner/v13/act/...`), not a separate module. |
| `act/runner` | Job planning and execution: `run_context.go` (env, binds, job container, host env), `step_action_remote.go` (remote actions), `step_docker.go`, `action.go`. |
| `act/container` | `docker/` (moby client, `run.go` creates containers, `sanitizeConfig` applies `valid_volumes`), `host_environment.go` (host executor). |
| `act/artifactcache` | Built-in actions cache server (`/_apis/artifactcache`, bolthold metadata, local disk storage, HMAC run validation in `mac.go`). |
| `act/cacheproxy` | Per-run proxy in front of the cache server. Gives each run a one-time URL prefix and signs requests with the shared secret. |
| `act/common/git` | Action repository cache (`Clone`, `Fetch`) and the `git()` exec helper with token auth through `--config-env http.<scheme>://<host>/.extraHeader=GIT_AUTH_HEADER`. |
| `act/plugin` | Experimental gRPC backend plugins (v13.1). A label scheme can name a plugin that provides the execution environment. |
| `testutils` | `RequireTestFeatures(t, docker|lxc)` and the `-features` test flag. Every test package imports it from `main_test.go`. |

### 1.2 Config sections that matter here

- `runner.envs`, `runner.env_file`: extra env for every job.
- `cache.*`: `enabled` (default true), `dir` (default `$HOME/.cache/actcache`), `port`, `proxy_port`, `host` (IP or name that job containers use to reach the runner; auto-detected with `common.GetOutboundIP()`), `external_server`, `secret`/`secret_url`, `actions_cache_url_override`.
- `container.options`: extra `docker run` flags for **every** container the job starts: the job container, service containers and docker actions.
- `container.valid_volumes`: glob allow-list. `docker/run.go:sanitizeConfig` drops every bind or mount whose source does not match, including binds from `container.options`. The runner always adds its own internal volume names and the Docker socket to the list.
- `container.workdir_parent` (default `workspace`): the job workspace is a **named Docker volume** at `/<workdir_parent>/<owner>/<repo>`, not a host bind (`BindWorkdir: false`).
- `container.docker_host`: `-` (none), `automount`, or a URL; mounted at `/var/run/docker.sock`.
- `host.workdir_parent` (default `$HOME/.cache/act`): the host job root, **and** the action repository cache (`ActionCacheDir`).

### 1.3 How a job gets its env

1. `NewRunner`: copy `runner.envs`, then add `GITHUB_SERVER_URL`, `ACTIONS_CACHE_URL`, `ACTIONS_RUNTIME_URL`, `ACTIONS_RESULTS_URL`, `FORGEJO_ACTIONS`, `FORGEJO_ACTIONS_RUNNER_VERSION`.
2. `Runner.run` (per task): copy those into `runEnvs`; add `ACTIONS_RUNTIME_TOKEN`, `ACTIONS_ID_TOKEN_REQUEST_*`; replace `ACTIONS_CACHE_URL` with the per-run proxy URL. The result goes into `runner.Config.Env`.
3. `RunContext.GetEnv`: `rc.Env = merge(workflow.env, job.env, Config.Env)`. **Runner env wins over workflow and job env.** Step env is merged later and wins over runner env.
4. The job container also gets `RUNNER_TOOL_CACHE=/opt/hostedtoolcache`, `RUNNER_OS`, `RUNNER_ARCH`, `RUNNER_TEMP=/tmp`, `LANG`.

Secrets (`task.Secrets` → `Config.Secrets`) are masked in logs.

### 1.4 Built-in actions cache

`run.SetupCache` starts `artifactcache` (unless `external_server`) and always a `cacheproxy`. Per run, `getWriteIsolationKey` gives an isolation key for `pull_request` events (unless the PR was closed by a merge). The proxy registers the run (`AddRun`) and the job gets `ACTIONS_CACHE_URL=<proxy>/<runID>/`. `RemoveRun` runs when the job ends. A PR run can read the shared cache but its writes stay in its own isolation scope. **The fork reuses this trust rule for the Turbo cache and the tool caches.**

### 1.5 Action fetching

`stepActionRemote` → `git.Clone(CloneInput{CacheDir: ActionCacheDir, URL, Ref, Token})`: one bare repo per action URL (`<cache>/<sha256(url)[:2]>/<rest>`), guarded by an in-process `common.MutexMap`. Since v13.2.0 (#1722) it fetches `--depth=1` of the requested ref only, and skips the fetch when the ref is a full SHA already present. Each step gets a temporary `git worktree`. This is already efficient. **Nothing to add.**

### 1.6 How `actions/checkout` runs

`NoSkipCheckout: true`, so `actions/checkout` is never short-circuited. It runs as a normal Node action inside the job container. It does `git init`, adds `origin`, then `git fetch --depth=1 origin +<sha>:refs/remotes/origin/<branch>` over smart HTTP with the task token (and `git submodule update` and `git lfs fetch` when asked). The runner does not help it in any way today. This is the gap the git mirror fills (section 3, U3).

### 1.7 Labels and executors

`runs-on` is matched to the runner labels. `docker://image` → job container. `host` → `-self-hosted` platform → `container.HostEnvironment` (steps run as host processes under `host.workdir_parent`). `lxc://...` → LXC through `lxc-helpers.sh` (also a "host env" in the code). A plugin scheme → `act/plugin`. `RunContext.IsHostEnv` tells host/LXC from docker.

### 1.8 Tests on Windows

- `go build ./...` works on Windows (go 1.26).
- `go test -short -count=1 ./... -args -features=-` disables the docker and LXC tests. On this Windows host, all packages pass **except three upstream failures that are not related to the fork**: `act/container/docker` (`TestParseRunLinks/bridge/onelink`, `TestParseNetworkConfig/invalid-mixed-network-types`) and `act/filecollector` (`TestFileCollector_CollectFiles/Symlinks_are_retained`).
- `act/common/git` (~55 s) and `act/runner` (~50 s) run real `git` and are slow but pass.
- `-args -features=-` fails a package whose tests do not define the flag. **Every new package must have a `main_test.go` that imports `code.forgejo.org/forgejo/runner/v13/testutils`**, as the upstream packages do.
- Format with `go run mvdan.cc/gofumpt@v0.12.0 -l <files>`. The full `make test` also runs golangci-lint v2.13.2.
- To scope tests: `go test -short -count=1 ./internal/pkg/<pkg>/... -args -features=-`, and use `-run` for `act/runner`.

---

## 2. What the runner already does (dropped from the fork)

| Wanted | Native mechanism | Fork action |
|---|---|---|
| `actions/cache` server with PR isolation | `artifactcache` + `cacheproxy` (1.4) | None. Keep using it for `target/` and similar caches. |
| Cache of action repositories | `act/common/git.Clone`, efficient since v13.2.0 (1.5) | None. |
| Docker socket in jobs | `container.docker_host` | None. |
| Host directory bind mounts with env | `container.options: -v /host:/ctr` + `container.valid_volumes` + `runner.envs` | Possible today with config only. U2 adds typed presets, directory creation, host-job path rewriting, trust gating, and keeps the mounts out of service containers. If those are not wanted, U2 can stay a documented config snippet. |
| Tool cache (`setup-node`, `setup-bun`) | **Not** persisted for docker jobs: `/opt/hostedtoolcache` is in the container layer. | Optional U2 preset `hostedtoolcache`, off by default. `@actions/tool-cache` removes and re-extracts the target directory, so two jobs that install the same version at the same time can break each other. |
| S3 backend for the Turbo cache | No S3 SDK in `go.mod` (no `minio-go`, no `aws-sdk-go`). | U1 ships the `Store` interface and the local disk backend only. An S3 backend (`minio-go/v7`, works with Garage) is a follow-up unit; the interface already allows it. |

---

## 3. Fork design

### 3.1 Principles

- One new top-level config section, `asc:`. Upstream sections keep their meaning.
- New code goes into new packages under `internal/`. The only edits to upstream files are: `act/runner/runner.go` (1 field), `act/runner/run_context.go` (about 6 lines), `internal/app/run/runner.go` (option and contributor call), `internal/app/cmd/daemon.go` and `job.go` (start and stop the asc services), `internal/pkg/config/config.go` (1 field and 1 `applyTo` call).
- Every feature is a **job contributor**: before the job starts it returns env vars, bind mounts and masked values; after the job ends it releases what it holds. The checkout providers, the Turbo cache, the tool caches and the asc socket are all contributors.
- A contributor never fails a job. On error it logs a warning to the job log and contributes nothing. The job then runs as it does upstream.
- Trust comes from the existing `getWriteIsolationKey`: no key → trusted; key (`refs/pull/N/head`) → untrusted PR scope.

### 3.2 Shared seam types

```go
// package jobenv (internal/pkg/jobenv)
type Job struct {
    ServerURL    string // Forgejo base URL, no trailing slash
    Repository   string // "owner/name"
    Owner        string
    Sha          string
    Ref          string
    EventName    string
    Token        string // task token; never logged
    Trusted      bool   // getWriteIsolationKey returned ""
    IsolationKey string // "" when trusted, else e.g. "refs/pull/12/head"
}

type Mount struct {
    Source   string // absolute host path
    Target   string // absolute path in the job container
    ReadOnly bool
}

type Contribution struct {
    Env    map[string]string // values use container paths
    Mounts []Mount
    Masked map[string]string // name → value, added to the job's secrets so logs mask them
}

type Contributor interface {
    Name() string
    Contribute(ctx context.Context, job Job) (Contribution, Release, error)
}

type Release func()

func Merge(parts ...Contribution) (Contribution, []string) // later wins for env; a duplicate mount target is dropped; warnings returned
func FirstOf(name string, cs ...Contributor) Contributor   // first contributor that returns no error and a non-empty contribution
```

Act side (`act/runner`):

```go
type JobMount struct { Source, Target string; ReadOnly bool }
// Config.JobMounts []JobMount
```

- `GetBindsAndMounts` appends `Source:Target[:ro]` binds and adds each `Source` to the valid-volume list. This applies to the job container and docker actions or steps (they share `GetBindsAndMounts`), not to service containers.
- `startHostEnvironment` rewrites env values for host jobs. A value that equals a mount `Target`, or starts with `Target + "/"`, is changed to the `Source` path. A `:`-separated list of absolute paths (for example `GIT_ALTERNATE_OBJECT_DIRECTORIES`) is rewritten item by item and joined with `os.PathListSeparator`. Host jobs get no binds.
- LXC and plugin backends: mounts are ignored and env is passed as for host jobs. This is documented as unsupported.

### 3.3 Config (`asc:` section)

```yaml
asc:
  turbo_cache:
    enabled: false
    dir: ""                    # default $HOME/.cache/turbocache
    max_size: 50GiB            # LRU cap on disk (docker/go-units syntax)
    max_artifact_size: 2GiB    # PUT limit
    port: 0                    # 0 = random
    host: ""                   # default: cache.host, else outbound IP
    team: ""                   # TURBO_TEAM; default = repository owner
    signature_key: ""          # optional TURBO_REMOTE_CACHE_SIGNATURE_KEY
    signature_key_url: ""      # file: URL, exclusive with signature_key
  tool_caches:
    enabled: false
    root: ""                   # host root, default $HOME/.cache/asc-toolcache
    untrusted: rw              # rw | none   (PR jobs)
    presets: [bun, cargo, uv, pip, npm, nuget, sccache]
    cargo_home: /root/.cargo   # where the cargo preset mounts registry/ and git/
    custom:
      - name: playwright
        target: /asc/cache/playwright
        env: { PLAYWRIGHT_BROWSERS_PATH: /asc/cache/playwright }
  checkout:
    providers: [asc, git_mirror] # tried in order; first success wins
  git_mirror:
    dir: ""                    # default $HOME/.cache/asc-git-mirrors
    target: /asc/git-mirrors # mount point in job containers (read-only)
    fetch_timeout: 5m
    lfs: true                  # also `git lfs fetch` the job commit into the mirror
    submodules: true           # also mirror same-host submodules (U3b)
  asc:
    binary: asc                 # looked up on PATH of the runner host
    timeout: 2m
    socket: /run/asc/asc.sock    # mounted when it exists on the host
    socket_target: /run/asc/asc.sock
    allowed_mount_roots: [/var/lib/asc]  # mounts that `asc ci checkout` may ask for
```

All sections are off by default. Host paths must be paths on the **Docker host**. If the runner itself runs in a container and talks to the host daemon over the socket, the runner's view and the Docker host's view of these paths must match (same as the existing `cache.dir` caveat).

### 3.4 U1: Turbo remote cache

Package `internal/pkg/turbocache`. HTTP server on the stdlib `net/http` mux (Go 1.22 patterns), started once per runner process like the cache server, on `asc.turbo_cache.port`.

| Endpoint | Behaviour |
|---|---|
| `GET /v8/artifacts/status` | `{"status":"enabled"}` |
| `HEAD /v8/artifacts/{hash}` | 200 found in the job's scope or the trusted scope, else 404 |
| `GET /v8/artifacts/{hash}` | Body plus `x-artifact-duration` and `x-artifact-tag` when stored. 404 on miss. Updates the access time used for LRU. |
| `PUT /v8/artifacts/{hash}` | Streams to a temp file, then renames it into place. Stores `x-artifact-duration`, `x-artifact-tag`, `x-artifact-sha`, `x-artifact-dirty-hash`. Checks `Content-Length` against `max_artifact_size`. Replies 202 `{"urls":["<hash>"]}`. Always writes to the job's own scope. |
| `POST /v8/artifacts` | `{"hashes":[...]}` → `{hash: {size, taskDurationMs, tag?} or null}` |
| `POST /v8/artifacts/events` | Accepts the array, counts HIT and MISS per source, returns 200. |
| `OPTIONS` (preflight) | 200 with allow headers, no redirect. |

- **Auth.** `Authorization: Bearer <token>`. The runner mints one random 32-byte token per job (`Issue(ns) (token, revoke)`), like `cacheproxy.AddRun`. The token maps to a namespace `{repository, scope}`. `teamId` and `slug` query parameters are ignored, so a token cannot reach another repository. An unknown or revoked token gets 401. Tokens are compared in constant time.
- **Scopes.** `trusted`, or `pr-<n>` from the isolation key. Reads try the job's scope, then `trusted`. Writes go to the job's scope only, so a PR cannot poison master's cache (same rule as `turbo-ci.md` 4.4).
- **Hash validation.** `^[A-Za-z0-9_-]{1,128}$`. Anything else gets 400. This also blocks path traversal.
- **Store interface:** `Stat`, `Get`, `Put` keyed by `(Namespace, hash)` with a `Meta{Size, DurationMs, Tag, Sha, DirtyHash, CreatedAt}`. Disk layout: `<dir>/<sha256(repo)[:16]>/<scope>/<hash[:2]>/<hash>` plus `<hash>.json`. On start, the store scans the directory to get the total size. After each `Put` that exceeds `max_size`, it evicts by oldest access time down to 90 % of the cap. Concurrent `Put`s of the same hash are safe because of temp file plus rename. The first rename wins, and artefacts are immutable.
- **Env injected** into every job (contributor): `TURBO_API=http://<host>:<port>`, `TURBO_TOKEN` (also in `Masked`), `TURBO_TEAM` (config or owner), and `TURBO_REMOTE_CACHE_SIGNATURE_KEY` when configured (also masked). Runner env wins over workflow and job env (1.3). A workflow can still turn the cache off at step level with `TURBO_CACHE=local:rw`.
- **asc seam.** Later, the asc daemon serves the same `/v8` API. Switching is a config change: turn `turbo_cache` off and let the asc provider return `TURBO_API` in its contribution. No workflow change.

### 3.5 U2: host toolchain caches

Package `internal/pkg/toolcache`. A pure function maps config to one `Contribution`. The runner creates the host directories at startup.

| Preset | Host dir | Target | Env |
|---|---|---|---|
| `bun` | `<root>/bun` | `/asc/cache/bun` | `BUN_INSTALL_CACHE_DIR` |
| `cargo` | `<root>/cargo/registry`, `<root>/cargo/git` | `<cargo_home>/registry`, `<cargo_home>/git` | none (moving `CARGO_HOME` would share `bin/` and `config.toml`) |
| `sccache` | `<root>/sccache` | `/asc/cache/sccache` | `SCCACHE_DIR` (the workflow still sets `RUSTC_WRAPPER`) |
| `uv` | `<root>/uv` | `/asc/cache/uv` | `UV_CACHE_DIR`, `UV_LINK_MODE=copy` |
| `pip` | `<root>/pip` | `/asc/cache/pip` | `PIP_CACHE_DIR` |
| `npm` | `<root>/npm` | `/asc/cache/npm` | `npm_config_cache` |
| `nuget` | `<root>/nuget` | `/asc/cache/nuget` | `NUGET_PACKAGES` |
| `hostedtoolcache` (opt-in) | `<root>/hostedtoolcache` | `/opt/hostedtoolcache` | none (see section 2 race) |

Concurrency: cargo (`.package-cache` lock), uv (file locks), npm (cacache, content-addressed) and bun (temp dir then rename) are safe for concurrent jobs. pip and nuget are safe enough in practice: atomic writes, and the worst case is a re-download. sccache's local disk cache works with many servers, but its size accounting is per server. Prefer turbo's sccache proxy over U1 later.

Hardlinks: the cache and the workspace are different mounts in the container, so bun, uv and git-lfs cannot hardlink and copy instead (`EXDEV`). That is still much faster than downloading.

Trust: `untrusted: none` mounts nothing for PR jobs. The default is `rw`, because ASC PRs come from org members. Bun does not re-verify integrity on a cache hit, so a hostile PR could poison the shared cache. Set `none` if the runner ever takes fork PRs.

### 3.6 U3: host git mirror

Package `internal/pkg/gitmirror`. A contributor and a checkout provider.

1. **Key.** `<dir>/<sha256(cloneURL)[:32]>.git`, plus an `asc-mirror.json` file with the URL for operators. `cloneURL = ServerURL + "/" + Repository + ".git"`.
2. **Update at job start.** Take the in-process `MutexMap` lock and an exclusive file lock (`x/sys`: `flock` on Unix, `LockFileEx` on Windows). If the repo is missing, `git init --bare` with `gc.auto=0` and a credential-free `origin`. Then fetch `+refs/heads/*:refs/heads/*`, `+refs/tags/*:refs/tags/*`, the job's ref (`refs/pull/N/head` for PRs) and the job SHA. Use `--prune` and the task token through `--config-env http.<scheme>://<host>/.extraHeader=GIT_AUTH_HEADER`, the same pattern as `act/common/git` (copied, about 30 lines, so `act` stays untouched). Time limit: `fetch_timeout`.
3. **LFS.** If `lfs` is on and `git lfs` exists on the host: `git lfs fetch origin <sha>` into the mirror. That fills `<mirror>/lfs/objects` for the job's tree only, not the whole 26 GB store.
4. **Hold.** Drop the exclusive lock and take a **shared** lock until the job ends (the `Release`). Maintenance (`git maintenance run --task=incremental-repack` or `git repack -ad`, a later CLI subcommand) takes the exclusive lock, so it never deletes a pack that a running job reads through alternates. Fetch only adds packs, so it is safe while jobs hold shared locks.
5. **Expose.** Mount `<mirror>` at `<target>/<key>.git`, read-only. Env:
   - `GIT_ALTERNATE_OBJECT_DIRECTORIES=<target>/<key>.git/objects` (`:`-joined when U3b adds submodule mirrors);
   - `GIT_CONFIG_COUNT=n`, `GIT_CONFIG_KEY_i=safe.directory`, `GIT_CONFIG_VALUE_i=<target>/<key>.git`. Git reads the alternate's refs as fetch "haves" by running `git --git-dir=<alt> for-each-ref`. The container user (root) does not own the mirror, so this keeps the ownership check from refusing it. Env config counts as command-line scope, which `safe.directory` trusts.
6. **Why this works with standard `actions/checkout`.** Every git process in the job inherits the env. `git fetch` advertises the mirror's ref tips as haves, so the server sends only objects the mirror lacks. When the job commit is already in the mirror (step 2 just fetched it), the transfer is close to empty. git-lfs reads `GIT_ALTERNATE_OBJECT_DIRECTORIES` too (`fs.resolveReferenceDirs`), looks in `<alt>/../lfs/objects`, and copies from there (`LinkOrCopyFromReference`) instead of downloading. `fetch-depth: 0` becomes cheap as well, because the history is already in the alternate. `deploy-affected` can keep `fetch-depth: 0` without the 2–5 min cost.
7. **Failure.** Any error → warning in the job log and an empty contribution. `actions/checkout` then fetches from the server as it does today.
8. **To verify during implementation** (acceptance test with real git over `file://`): the number of objects the job repo receives when the commit is already mirrored, with `--depth=1` and with full depth. If shallow fetches still send the whole tree, recommend `fetch-depth: 0` with the mirror and note it in ASC.md.

**U3b: submodules.** With `submodules: true`, the mirror reads `.gitmodules` at the job SHA (`git show <sha>:.gitmodules`). It resolves relative URLs (`../soma.git` → `<server>/asc/soma.git`), keeps same-host URLs only, and mirrors each one (default branch plus the gitlink SHA from `git ls-tree <sha> <path>`). It adds their `objects` dirs to the alternates list and their paths to `safe.directory`. Errors per submodule are warnings. The task token may not be able to read the submodule repository. Then that mirror is skipped.

### 3.7 U4: checkout providers and the asc seam

Package `internal/pkg/checkout`:

- `providers: [asc, git_mirror]` builds `jobenv.FirstOf("checkout", ...)`. The first provider that succeeds wins. An empty list turns the chain off.
- **asc provider.** `exec.LookPath(binary)`. If it is missing → `ErrUnavailable`, and the chain goes on to the next provider. Otherwise run `asc ci checkout --server <url> --repo <owner/name> --sha <sha> --ref <ref> --trusted=<bool> --format=json`, with `ASC_TOKEN` in the child env (never in argv) and a `timeout` limit. Stdout must be `{"env":{...},"mounts":[{"source":"...","target":"...","read_only":true}]}`. Each mount source must be absolute and under `allowed_mount_roots`, or the whole result is rejected. A non-zero exit, bad JSON or a rejected mount → error → fall back.
- **asc socket contributor**, independent of the chain. If `socket` exists on the host: mount it at `socket_target` and set `ASC_DAEMON_SOCKET=<socket_target>`. This is the hook for the host daemon in `turbo-ci.md` 4.1. `rshared` FUSE propagation is not in scope. When asc needs it, it goes into the same `Mount` type as a `Propagation` field.
- The `act/plugin` backend (v13.1) is another possible asc seam later: an asc-aware execution environment. It is out of scope now.

### 3.8 Wiring

- `internal/app/asc`: `Start(cfg *config.Config) (*Services, error)` starts the Turbo server, creates the directories, and builds the contributor list in a fixed order: `turbo`, `toolcaches`, `checkout` chain, `asc-socket`. `(*Services).Contributors()` and `Close()`.
- `daemon.go` and `job.go`: start after `SetupCache`, close on exit, and pass `run.WithContributors(svc.Contributors()...)` through `createRunner` to `run.NewRunner(..., opts ...run.Option)`.
- `Runner.run`: build `jobenv.Job` (trust from `getWriteIsolationKey`; an error there counts as untrusted, with a warning), call the contributors, `Merge`, copy env into `runEnvs`, mounts into `runner.Config.JobMounts`, masked values into a **copy** of `task.Secrets` (never overwriting a task secret), `defer` the releases, and log one line per contributor to the job log.

### 3.9 Rebase on upstream

`asc` is a branch of commits on top of an upstream tag. To move to a new release:

```sh
git fetch origin --tags
git rebase --onto vX.Y.Z v13.2.0 asc   # old base → new base
go build ./... && go test -short -count=1 ./... -args -features=-
```

Conflicts can occur only in the files listed in 3.1. The `act` patch is small and isolated (`act/runner/job_mounts.go` plus a few call-site lines). Keep it that way, and do not edit other `act` files.

---

## 4. Units

| Id | Unit | Depends on | Main files |
|---|---|---|---|
| S1 | act job mounts | – | `act/runner/job_mounts.go`, `runner.go`, `run_context.go` |
| S2 | asc config schema | – | `internal/pkg/config/asc.go`, `config.go`, `config.asc.example.yaml` |
| S3 | jobenv seam and wiring | S1, S2 | `internal/pkg/jobenv/`, `internal/app/asc/`, `internal/app/run/runner.go`, `internal/app/cmd/daemon.go`, `job.go` |
| U1 | Turbo remote cache | S3 | `internal/pkg/turbocache/` |
| U2 | host toolchain caches | S3 | `internal/pkg/toolcache/` |
| U3 | host git mirror and LFS | S3 | `internal/pkg/gitmirror/` |
| U3b | submodule mirrors | U3 | `internal/pkg/gitmirror/submodules.go` |
| U4 | checkout providers, asc stub and socket | S3 (U3 for the chain test) | `internal/pkg/checkout/` |
| U5 | docs | all | `ASC.md`, `config.asc.example.yaml` comments |

Follow-ups, not units yet: an S3 or Garage `Store` for U1 (adds `minio-go/v7`); a `forgejo-runner asc maintain` command (mirror repack under the exclusive lock, turbo cache eviction report); an asc-aware `act/plugin` backend.
