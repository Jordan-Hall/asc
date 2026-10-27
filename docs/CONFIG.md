# asc configuration

asc reads git's configuration files (system, `~/.gitconfig`, the XDG file, and the repository's
`config`), with `includeIf` evaluated as git does, then `-c key=value` on the command line. Its own
keys live under `asc.*`. Set them with `asc config`; every example below works as written.

```sh
asc config --global asc.live.update true       # your account, every repository
asc config asc.stack.trunk release/1.2         # this repository only (appends: trunk is multi-valued)
asc config --get asc.live.update               # read the effective value
```

Scopes: **user** means a key that only your own user or system config may set; a repository's
`config` is ignored for it. A repository config that you do not own cannot set the keys that
choose a program or redirect where asc reads and writes (`asc.realGit`, `asc.home`,
`asc.allowLoopbackHttp`, `asc.cache.dir`, `asc.cache.readOnly`), the same rule git applies. Sizes
take a number with a suffix such as `500MiB`; durations take a number with `s`, `m`, `h` or `d`.

The commands that read these keys are in [`CLI.md`](CLI.md); the index of all docs is
[`README.md`](README.md).

## Installation and paths

| Key | Default | Meaning | Example |
|---|---|---|---|
| `asc.realGit` | the `git` found on `PATH`, past asc's own shim | The real git binary asc runs for hooks, credential helpers and `!` aliases. Not settable from a repository you do not own. | `asc config --global asc.realGit /usr/bin/git` |
| `asc.home` | the per-user asc data directory | Where asc keeps its state, store and cache. Not settable from a repository you do not own. | `asc config --global asc.home ~/.asc` |
| `asc.cache.dir` | `cache/` inside `asc.home` | The writable object cache. A CI host points every job at one warm cache. | `asc config --global asc.cache.dir /var/cache/asc` |
| `asc.cache.readOnly` | none | A cache directory that is read, never written. Repeatable; a PR job mounts the warm cache this way. | `asc config --add asc.cache.readOnly /var/cache/asc` |
| `asc.cache.maxSize` | the daemon's default | The size the daemon keeps the cache under (`asc.cache.maxsize` is accepted too). | `asc config --global asc.cache.maxSize 20GiB` |
| `asc.releasesUrl` | none | The https release directory `asc upgrade`, `asc self-update` and `asc codeintel enable-embed` read, in place of `ASC_DOWNLOAD_BASE_URL`. | `asc config --global asc.releasesUrl https://downloads.example.org/asc` |
| `asc.shim.init` | unset | With the value `asc`, `git init` run through the shim creates an asc repository. | `asc config --global asc.shim.init asc` |
| `asc.excludesFile` | `~/.config/asc/ignore` | Your global ignore file, read before `core.excludesFile`; a repository's `.ascignore` rules still apply. | `asc config --global asc.excludesFile ~/.config/asc/ignore` |
| `asc.displayIds` | `git` | The ids shown in human output when you run `asc`: `git`, `asc` or `both`. Never changes `--porcelain`, `-z`, `--format` or plumbing output. | `asc config --global asc.displayIds both` |
| `asc.turboIndex` | `auto` | Whether a workspace gets a synthetic `index` file for Turborepo: `true`, `false`, or `auto` (on when the root has `turbo.json` or the workspace is plain). | `asc config asc.turboIndex true` |
| `asc.allowLoopbackHttp` | `false` | Allows plain `http://` to `127.0.0.1`, `localhost` and `[::1]` for local test servers. Any other http URL is always refused. Also `ASC_ALLOW_LOOPBACK_HTTP=1`. Not settable from a repository you do not own. | `asc config --global asc.allowLoopbackHttp true` |

## Servers and history

| Key | Default | Meaning | Example |
|---|---|---|---|
| `asc.server` | the origin of `remote.origin.url` | The asc server `asc affected --repo` and `asc ci` talk to. The environment variable `ASC_SERVER` wins. | `asc config asc.server https://asc.example.org` |
| `asc.serverHistory` | `auto` | `always` makes `log` and `blame` ask the server for history; otherwise the local engine answers inside the history window and continues past it through the server. | `asc config asc.serverHistory always` |
| `asc.fetch.historyWindow` | `10000` | How many generations of history `clone` and `fetch` download below each tip. | `asc config asc.fetch.historyWindow 50000` |
| `asc.fetch.ancestryBudget` | `100000` | The most commits a fetch walks to decide what the server must send. | `asc config asc.fetch.ancestryBudget 200000` |
| `asc.gc.pitrDays` | `14` | Store garbage collection keeps ref-log targets newer than this many days. It must be at least the database's point-in-time-recovery window. | `asc config asc.gc.pitrDays 30` |
| `asc.opLog` | `true` | `false` stops recording operations, so `asc undo` and `asc op` have nothing to work from. | `asc config asc.opLog false` |
| `asc.opLog.maxDays` | `90` | Operations older than this are pruned. | `asc config asc.opLog.maxDays 30` |
| `asc.opLog.maxOps` | `1000` | Operations beyond this many are pruned. | `asc config asc.opLog.maxOps 5000` |

## Stacks and review

| Key | Default | Meaning | Example |
|---|---|---|---|
| `asc.branchless` | `true` | Client behaviour: cloud upload, `asc create` without a label, and `push` submitting the stack. Also `ASC_BRANCHLESS`. It never changes local git output. | `asc config --global asc.branchless false` |
| `asc.stack.trunk` | the default branch | The branches stacks are built on. Multi-valued. | `asc config --add asc.stack.trunk release/1.2` |
| `asc.stack.immutable` | `refs/remotes/*/release/*` | Refs whose ancestors asc never rewrites. Multi-valued; tags are not checked. | `asc config --add asc.stack.immutable 'refs/remotes/*/stable'` |
| `asc.stack.branchTemplate` | `{user}/{slug}` | The name `asc create` and `asc submit` give a new branch, from `user.name` and the change's subject. At most 128 characters and not starting with `-`. | `asc config asc.stack.branchTemplate 'stack/{slug}'` |
| `asc.stack.submit.draft` | `true` | Whether `asc submit` opens drafts unless you pass `--publish`. | `asc config asc.stack.submit.draft false` |
| `asc.push.review` | `draft` | The remote effect of `push` on a stack: `draft`, `publish` or `none`. | `asc config asc.push.review publish` |
| `asc.draft.hiddenDays` | `90` | How long a hidden draft is kept locally. Largest accepted value 3650. | `asc config --global asc.draft.hiddenDays 30` |

## Commit cloud

The cloud uploads your drafts in the background so they follow you between machines.

| Key | Default | Meaning | Example |
|---|---|---|---|
| `asc.cloud` | `auto` | `auto` is on when `asc.branchless` is on, the server offers the cloud for the repository and a daemon runs; `on`; or `off`. A public repository never uploads. | `asc config asc.cloud off` |
| `asc.cloud.workspace` | `default` | This machine's cloud workspace: `default`, `host` (the host name) or a name. | `asc config asc.cloud.workspace host` |
| `asc.cloud.maxSyncAge` | `14d` | Heads older than this are not listed by `asc cloud list` without `--all`. | `asc config --global asc.cloud.maxSyncAge 30d` |
| `asc.cloud.queueMax` | `10000` | The longest upload queue; beyond it new heads stay local and `asc cloud status` says `queue full`. | `asc config --global asc.cloud.queueMax 2000` |
| `asc.cloud.maxChangesetSize` | `1GiB` | The largest changeset uploaded. | `asc config --global asc.cloud.maxChangesetSize 256MiB` |
| `asc.cloud.denyPaths` | `.env*`, `*.pem`, `*.key`, `id_*`, `*credentials*` | Paths held back from upload until `asc cloud release`. | `asc config --global asc.cloud.denyPaths '*.p12'` |

## Live updates

The daemon keeps a worktree current while you work. `asc live pause` stops it for one worktree.

| Key | Default | Meaning | Example |
|---|---|---|---|
| `asc.live.fetch` | `true` | Fetch when the server's trunk moves. | `asc config asc.live.fetch false` |
| `asc.live.restack` | `false` | Restack your stack onto the new trunk after a fetch. | `asc config asc.live.restack true` |
| `asc.live.update` | `false` | Also move the working copy to the restacked or new-version commit of the checked-out change, or to the new trunk tip when you are on trunk. It never moves a dirty working copy or one with an operation in progress; `asc undo` reverts it. | `asc config asc.live.update true` |
| `asc.live.debounce` | `2s` | Wait this long after an event before a live pass. | `asc config --global asc.live.debounce 5s` |
| `asc.live.pollInterval` | `5m` | Poll the server this often when no event arrives. | `asc config --global asc.live.pollInterval 10m` |
| `asc.live.maxStack` | `50` | Above this many changes nothing is restacked and a notice is raised. | `asc config --global asc.live.maxStack 20` |
| `asc.live.maxFiles` | `5000` | Above this many changed paths nothing is restacked and a notice is raised. | `asc config --global asc.live.maxFiles 2000` |

## Virtual file system (Windows)

| Key | Default | Meaning | Example |
|---|---|---|---|
| `asc.vfs.hydrationBudget` | `500MiB` | Clean downloaded files are turned back into placeholders once a workspace holds more than this. | `asc config --global asc.vfs.hydrationBudget 2GiB` |
| `asc.vfs.denyHydration` | `SearchProtocolHost.exe;SearchIndexer.exe;SearchFilterHost.exe` | Programs whose reads do not download files. A `;` separated list of executable names. | `asc config --global asc.vfs.denyHydration 'SearchIndexer.exe;MsMpEng.exe'` |

## MCP (`asc mcp`)

The write keys are **user** keys: a repository config or a command-line flag can only narrow what
they grant. See [`mcp.md`](mcp.md).

| Key | Default | Meaning | Example |
|---|---|---|---|
| `asc.mcp.write` | `false` | `false`, `local` (verbs with no network effect) or `remote` (also verbs that upload or contact the network). User only. | `asc config --global asc.mcp.write local` |
| `asc.mcp.confirm` | `destructive` | When a write tool asks you first: `always`, `destructive` or `never`. User only. | `asc config --global asc.mcp.confirm always` |
| `asc.mcp.toolsets` | `core` | Comma list from `core,history,stack,projects,search,write`, or `all`. | `asc config --global asc.mcp.toolsets core,search` |
| `asc.mcp.tools` | none | An explicit tool list, intersected with the toolsets and the write gate. See [`mcp.md`](mcp.md) for the names. | `asc config --global asc.mcp.tools status,log` |
| `asc.mcp.maxTokens` | `6000` | Per-call output budget; the hard cap is 25000. | `asc config --global asc.mcp.maxTokens 12000` |
| `asc.mcp.textMode` | `git` | The text form of results: `git` or `json`. | `asc config --global asc.mcp.textMode json` |

## Environment variables

| Variable | Meaning |
|---|---|
| `ASC_TOKEN` | The token sent to the remote's own origin, never to another host. Also read by `asc ci` steps. |
| `ASC_SERVER` | The server for `asc affected --repo` and `asc ci`; it wins over `asc.server`. |
| `ASC_REMOTE` | The remote name or URL `asc ci cache` and `asc ci artifact` use. |
| `ASC_HOME` | The data directory (the same as `asc.home`). |
| `ASC_NO_DAEMON` | Run without the background daemon. |
| `ASC_NO_ALIASES` | Set to `1` to ignore `alias.*` entries. The editor plugins' bridge sets it. |
| `ASC_BRANCHLESS` | The same as `asc.branchless`. |
| `ASC_ALLOW_LOOPBACK_HTTP` | The same as `asc.allowLoopbackHttp`. |
| `ASC_DOWNLOAD_BASE_URL` | The release directory the installers and `asc upgrade` read. |
| `ASC_RELEASE_PUBKEY` | The minisign public key; the installers then require a signed `SHA256SUMS`. |
| `ASC_HOST_CACHE` | The host cache directory for `asc ci checkout`. |
| `ASC_SERVE_CONFIG` | The `asc-serve.toml` path for `asc serve`, when `--config` is not given. |

The server reads its own file, `asc-serve.toml`; see [`../deploy/README.md`](../deploy/README.md).
