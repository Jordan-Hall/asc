# Task runner (`asc run`)

Status: design, 2026-11-13. Roadmap row 24 (`docs/ROADMAP-NEXT.md`). Decided on 2026-11-13: asc gets
a task runner, as an optional module that is installed and set up separately from the core `asc`
binary. Nothing here is built yet. SPEC §0 lists "`asc run` task execution" as out of scope for the
core; this document keeps it out of the core and puts it in a module.

## Why

asc already knows three things that a monorepo task runner has to work out for itself:

- **The project graph.** `asc-graph` detects Rust, Python, .NET, Node and declared projects and
  their edges from the tree (SPEC §12A). Turborepo and Nx see only the JavaScript part of it.
- **Content hashes without reading files.** A tree entry carries the content hash of its file, so
  the per-project input hash of SPEC §12A.6 costs one tree id per unchanged directory and never
  reads a file. A runner that hashes files itself reads every input on every run.
- **A build cache with a server tier.** `asc-buildcache` is one machine-wide content-addressed
  store, shared by every adapter, with remote restore and upload through `asc serve`
  (`docs/designs/build-cache.md` §2, §3).

`asc run` joins these: it runs the tasks of the affected projects in graph order, keys each task by
the hashes asc already has, and restores a task from the cache instead of running it when its key
is known.

Non-goals:

- A build system. `asc run` runs commands that the repository already has (package.json scripts,
  cargo commands, declared commands). It does not model compiler actions; the cargo, Go, .NET and
  JavaScript adapters of the build cache stay the way to cache inside a tool.
- Hermetic builds. The runner promises what it can check (section "Hermeticity and sandboxing")
  and says plainly what it cannot.
- Replacing Turborepo or Nx for teams that use them. Those keep working with the asc cache adapter
  (`asc cache setup turbo`, `asc cache setup nx`); `tests/turbo` stays the contract for them.

## What exists and what the runner adds

### Reused as it is

Each item names the piece, where it is, and what the runner uses it for.

- **Project model.** `crates/asc-graph/src/model.rs:155` (`Project`), `:215` (`ProjectGraph`): the
  nodes and edges that tasks hang off.
- **Project config.** `crates/asc-graph/src/config.rs:80` (`ProjectEntry`), `:96`
  (`ResolvedConfig`): named inputs, `[targets.*]` input lists, tags.
- **Target commands.** `crates/asc-graph/src/targets.rs:87` (`commands_for`), `:28`
  (`check_argv`): the A4 `argv` table, a declared command per target, already bounded.
- **Topological order.** `crates/asc-graph/src/affected.rs:632` (`topo_order`), `:573` (`sccs`),
  `:538` (`adjacency`): dependencies first, a cycle as one component.
- **Affected.** `crates/asc-graph/src/affected.rs:128` (`affected`), `:89` (`AffectedProject`):
  the projects a change reaches, with reasons, for `--affected`.
- **Input hashes.** `crates/asc-engine/src/projects/hash.rs:117` (`input_hashes`),
  `crates/asc-graph/src/model.rs:487` (`InputHashes`), `:490` (`phash_key`): the project half of
  the task key, from trees only, memoised as `phash.v1`.
- **Affected CLI.** `crates/asc/src/cli/affected.rs:476`: `asc affected --json --hashes --target
  <t>` already returns those hashes.
- **Work-tree inputs.** `crates/asc/src/cli/inputs.rs:364` (`compute`), `:87` (`digest`): the
  dirty overlay; only changed and untracked files are read.
- **Action keys.** `crates/asc-buildcache/src/key.rs:16` (`ActionKey::builder`), `:46`
  (`KeyBuilder`): length-prefixed, ordered key material; tools never share entries.
- **Store.** `crates/asc-buildcache/src/store.rs:52` (`Action`), `:663` (`lookup`), `:683`
  (`restore`), `:517` (`put_outputs`), `:553` (`put_action`): local lookup, restore and write.
- **Incremental restore.** `crates/asc-buildcache/src/sync.rs:33` (`Store::sync`): a restore writes
  only the files that differ, so watchers see only real changes.
- **Outbox.** `crates/asc-buildcache/src/pack.rs:146` (`queue_push`): queue a key for
  `asc cache push`.
- **Output safety.** `crates/asc-buildcache/src/store.rs:206` (`check_rel`), `:225`
  (`check_link`), `:253` (`check_paths`): refuse entries whose outputs escape the root.
- **Remote tier.** `crates/asc/src/cli/cache/remote.rs:159` (`fetch`),
  `crates/asc-serve/src/turbo.rs:1072` (`POST /v8/asc/build-revs/<rev>`): scoped artefacts and
  per-revision key lists.
- **Cached command.** `crates/asc/src/cli/cache/run.rs:96` (`key`), `:213` (`run_teed`), `:246`
  (`replay`): the model for one cached task: key, tee, replay, store exit 0 only.
- **Command sandbox.** `crates/asc-ci/src/sandbox.rs:68` (`Config`), `:107` (`Spec`), `:147`
  (`MASK`): no shell, scrubbed environment, timeout, output cap, secret masking, optional wrapper.
- **External commands.** `crates/asc/src/dispatch.rs:349` (`resolve`), `:585` (`external`), `:929`
  (`run_external`): `asc run` already resolves to `asc-run` on `PATH`, with no core change.
- **Signed install.** `crates/asc/src/cli/upgrade.rs:129` (`verify`), `:365` (`install`), `:113`
  (`client_too_old`): minisign-verified, side-by-side install, reused for the module.
- **Doctor.** `crates/asc/src/cli/doctor.rs:1697` (`gather`), `:1763` (`checks_json`): one more
  check line for the module.
- **MCP catalogue.** `crates/asc-mcp/src/catalogue.rs`, `crates/asc-mcp/tools.toml`: the pattern
  for the module's own tool list.
- **CI insights.** `crates/asc-serve/src/ci/insights/`, `crates/asc-ci/src/insights/flaky.rs:74`
  (`analyse`): flaky detection over task results the server already stores.

Two facts shape the design:

- `asc` is a binary crate. The input overlay (`cli/inputs.rs`) and the remote fetch
  (`cli/cache/remote.rs`) live inside it, so a separate binary cannot link them. The module reaches
  them through the `asc` command and its JSON output, which is also the path that keeps the store
  and protocol versions owned by one program.
- `asc-graph`, `asc-buildcache` and `asc-core` are plain libraries with few dependencies. The
  module links them directly for the pure parts: the graph types, topological order, action keys
  and the local store layout (a set of files that many processes already share, `lib.rs:12`).

### Over an existing runner with the asc cache adapter

Turborepo or Nx with `asc cache setup turbo|nx` already get the shared store and the server tier.
What `asc run` adds:

1. **Polyglot graph.** A Rust crate that a Node package depends on through a napi build, a Python
   package and a declared `migrations` project are all nodes with edges. Turborepo sees only
   workspaces in package.json.
2. **No file reads for clean inputs.** Turborepo hashes files through the synthetic index of
   SPEC §9.4 or by reading them; `asc run` takes `files_digest` from tree ids. A clean checkout of
   a large repository keys every task without opening a source file.
3. **Affected against a changeset, not a local diff.** `--affected --base <rev>` uses the same
   algorithm as `asc affected` (SPEC §12A.7), including global inputs, eco-global files, lockfile
   changes and gitlinks, and works on the server's data without a full history.
4. **One key space with the other adapters.** A task entry and a `rustc` entry live in one store,
   one eviction policy and one remote scope model; `asc cache status` shows both.
5. **Agent output.** Every run can emit stable JSON events, and an MCP server exposes the same
   operations.
6. **No Node.js requirement.** The module is one static binary.

Teams that already have `turbo.json` keep it: the runner can import its `dependsOn` and `outputs`
(open question 3), and turbo itself stays supported.

## Packaging as an optional module

### Crate and binary

- New crate `crates/asc-run`, package `asc-run`, one library target (`asc_run`) and one binary
  target `asc-run`. It is a workspace member so it shares lints, `unsafe_code = "deny"` and the
  dependency versions of `Cargo.toml`.
- Dependencies: `asc-core`, `asc-graph`, `asc-buildcache`, `asc-sandbox` (unit 5), `serde`,
  `serde_json`, `toml`, `blake3`, `tokio`, `clap`, and `rmcp` for the MCP server.
- **The core never depends on it.** `crates/asc/Cargo.toml` has no `asc-run` dependency, no feature
  that names it and no code that imports it. A test in `crates/asc-run/tests/layering.rs` reads
  every `crates/*/Cargo.toml` except its own and fails when any of them names `asc-run`.
- The core release archive does not contain `asc-run`. It is a separate archive
  `asc-run-<ver>-<target>.zip` with its own `.minisig`, signed by the same release key.

### Discovery: external subcommand

The dispatcher already resolves an unknown word under `asc` as builtin, asc-only verb, alias,
`asc-<name>`, then `git-<name>` (`dispatch.rs:349`, SPEC §11.4). `run` is neither a git builtin nor
in `ASC_ONLY_VERBS` (`dispatch.rs:230`), so `asc run test` already executes `asc-run test` when
`asc-run` is on `PATH`, through `run_external` (`dispatch.rs:929`), which forwards the exit code.

Three small core changes complete the mechanism (unit 1):

1. **A module directory.** `Lookup::external` also searches `<install prefix>/modules/bin`, after
   `PATH`, for the `asc-` prefix only. `asc module install` puts binaries there, so no `PATH` edit
   is needed. `PATH` stays first, so a developer build of `asc-run` wins over an installed one.
2. **`run` is reserved for the module.** A dispatch test pins that `run` is never added to
   `ASC_ONLY_VERBS` or the builtins, and that `resolve(Program::Asc, "run")` is `External` when
   `asc-run` exists. When it does not, `asc run` prints `asc: 'run' is an optional module;
   install it with 'asc module install run'` and exits 1, instead of the generic "not an asc
   command".
3. **Context for the child.** `run_external` already starts the child with the context of `Ctx`.
   It also sets `ASC_EXE` (the absolute path of the running `asc`), so the module calls the same
   core that dispatched it rather than whatever `asc` is first on `PATH`.

`ASC_NO_ALIASES=1` turns external lookup off (`dispatch.rs:586`). The module honours that: under
`ASC_NO_ALIASES=1` the user runs `asc-run` directly.

### Installation

- **`asc module install run [--version <v>] [--from <dir>]`**, a new asc-only verb `module` with
  `install`, `remove`, `list` and `info`. The source is `asc.releasesUrl` (https only), the same as
  `asc upgrade`. It downloads `asc-run-<ver>-<target>.zip` and its signature, verifies with the
  baked-in release key (`upgrade.rs:129`) before unpacking, unpacks to
  `<prefix>/modules/run/<ver>/`, and links `<prefix>/modules/bin/asc-run[.exe]` to it (a junction or
  symbolic link; a copy on file systems without either). Old versions are pruned at the next
  install, as `upgrade.rs:391` does.
- **Package managers.** Separate packages, never pulled in by the core package:
  `packaging/homebrew/asc-run.rb.in` (`depends_on "asc"`), `packaging/scoop/asc-run.json.in`
  (`"depends": "asc"`), `packaging/winget/asc.asc-run.*.yaml` (dependency on `asc.asc`),
  `packaging/aur/PKGBUILD-asc-run.in`, and `packaging/linux/nfpm-asc-run-<arch>.yaml.in`
  (`depends: [asc]`, installs `/usr/bin/asc-run`). Each puts `asc-run` on `PATH`, which the
  dispatcher finds first.
- **Cargo.** `cargo install --path crates/asc-run` for developers.
- **Setup.** The module has no daemon and no service. `asc run init` writes a starter
  `.asc/tasks.toml` from the package.json scripts and cargo targets it finds, and prints it; it
  never edits other files.

### Version compatibility

- The core exposes a **module API version**, an integer, in `asc module info --json`
  (`{"asc": "<ver>", "module_api": 1, "buildcache_layout": 1}`). The module API is the set of core
  commands and JSON shapes the module calls (listed in section "Execution", "Core calls"). It goes
  up only when one of them changes incompatibly.
- `asc-run` declares the range it supports (`MODULE_API = 1..=1`) and checks it at start through
  `$ASC_EXE module info --json`. Out of range: exit 2 with `asc-run <v> needs module API <range>;
  asc <v> has <n>; run 'asc module install run' or 'asc upgrade'`.
- `asc-buildcache` is linked into both binaries. Its on-disk layout already has versioned keys
  (`KEY_CONTEXT = "asc build action v1"`, `key.rs:7`) and a versioned artefact header (`ASCBC01`).
  The module refuses to write to a store whose layout version it does not know.
- `asc upgrade` checks installed modules after switching versions and prints one line per module
  whose range no longer includes the new API, with the install command. It never upgrades a module
  by itself.

### `asc doctor`

`asc doctor` gains one check per known module, from the module directory and `PATH`, with no
module code loaded (the probe rules of `doctor.rs:10` apply: absolute path, null stdin, 10 s):

```
info module run: not installed (optional; asc module install run)
ok   module run: asc-run 0.3.1 at <prefix>/modules/bin/asc-run (module API 1)
warn module run: asc-run 0.2.0 needs module API 2, asc has 1
  fix: asc module install run
warn module run: two copies; PATH has /usr/bin/asc-run 0.3.1, which wins over <prefix>/modules/bin
```

A missing module is `info`, never `warn` or `fail`. `asc doctor --json` reports the same through
`checks_json`.

## Configuration

Tasks come from three sources, merged per project in this order (a later source replaces an
earlier one's task of the same name):

1. **Ecosystem defaults** (`crates/asc-run/src/sources/`):
   - Node: each key of `scripts` in the project's `package.json` is a task, run as
     `<package manager> run <script>` with the package manager from `packageManager` (the field the
     node detector already keeps, `crates/asc-graph/src/detect/node.rs:23`). The detector drops
     `scripts` from its parsed value (`node.rs:984`), so the runner reads `package.json` itself.
   - Rust: `build`, `test`, `check`, `clippy` and `doc` as `cargo <verb> -p <crate>`, with the
     project root as cwd. Cargo aliases in `.cargo/config.toml` are not expanded.
   - Python, .NET: none by default; declared in `.asc/tasks.toml`.
2. **The A4 `argv` table** of `.asc/projects.toml` (`config.rs:91`): `argv = { test = [...] }` on a
   `[[project]]` is a task with that command.
3. **`.asc/tasks.toml`**, optional, read from the revision being run (like `.asc/projects.toml`).
   The repository keeps its configuration under `.asc/`, and `docs/designs/build-cache.md:452`
   records that there is no top-level `asc.toml`, so the task section is a file in `.asc/`.

```toml
version = 1

[defaults]
env = ["CI", "NODE_ENV"]               # values in every task key
pass_env = ["HOME", "CARGO_HOME"]      # passed to the command, not in the key
timeout = "30m"

[tasks.build]                          # applies to every project that has a build command
depends_on = ["^build"]                # build of each dependency first
outputs = ["dist/**", "target/release/*.node"]
inputs = ["production", "^production"] # named inputs; default: [targets.build] of projects.toml

[tasks.test]
depends_on = ["build"]                 # build of the same project first
outputs = []
env = ["DATABASE_URL"]                 # adds to [defaults] env
cache = true

[tasks.lint]
command = ["bunx", "oxlint", "."]      # one command for every project that runs lint
select = ["node:*"]                    # projects the task applies to (asc-graph selectors)

[[project]]
select = "rust:asc-core"
tasks.test = { command = ["cargo", "test", "-q", "-p", "asc-core"], timeout = "1h" }

[[project]]
select = "declared:migrations"
tasks.check = { command = ["sqlfluff", "lint", "."], outputs = [] }
```

- **Commands** are argument lists, never shell strings. A package.json script is the one place a
  shell runs, because the package manager runs it; that is the package manager's behaviour, and the
  runner passes no extra text through it.
- **Inputs** reuse the pattern grammar of SPEC §12A.3 (`{projectRoot}`, `{workspaceRoot}`, named
  inputs, `^name`, `!` exclusions). A task with no `inputs` uses `[targets.<task>]` of
  `.asc/projects.toml`, and that defaults to `["default", "^default"]`. Ignored files are never
  inputs, so outputs under the project root do not change the key.
- **Outputs** are repository-relative globs under the project root, checked when the file is read:
  no `..`, no absolute path, no output inside another task's outputs. An output that matches a
  tracked file is an error (`outputs must be ignored files: <path> is tracked`), because restoring
  it would edit the tree.
- **Environment.** `env` names variables whose values enter the key; `pass_env` names variables
  passed through without entering the key. Every other variable is removed (section "Hermeticity").
  A name in `env` or `pass_env` that looks like a secret (`*TOKEN*`, `*SECRET*`, `*PASSWORD*`,
  `*_KEY`, `*CREDENTIAL*`) must be listed under `secret_env` instead (section "Security").
- **Dependencies.** `depends_on` items are `<task>` (same project), `^<task>` (that task of every
  dependency project, all edge kinds) and `<project id>#<task>` (one named project). A task that
  does not exist in a dependency is skipped for that dependency. A cycle among tasks is an error
  that names the cycle; a cycle among projects (an SCC of `sccs`) runs the members' task in display
  id order and treats `^task` inside the component as "none".
- **Bounds.** The file is untrusted text from a branch: at most 256 KiB, 512 tasks, 64 `[[project]]`
  entries per selector, the `targets.rs` limits for every command, and 256 globs per list.

`.asc/tasks.toml` is a global input of every task key, the way `.asc/projects.toml` is a global
input of `asc affected` (SPEC §12A.7 step 3a).

## Execution

### Core calls

The module calls the core for everything the core owns. Each call is one `$ASC_EXE` child with
`--json`, and each is part of module API 1:

| Call | Returns | Used for |
|---|---|---|
| `asc module info --json` | versions | Compatibility check |
| `asc projects --format json` | the project graph of the work tree | Tasks per project, edges |
| `asc inputs --project <id> --target <t> --json` | §12A.6 hash, dirty overlay | Key (unit 2) |
| `asc affected --base <rev> --json --hashes --target <t>` | projects, reasons, hashes | Affected |
| `asc cache fetch <key>... --json` (unit 3) | per key: fetched, absent, refused | Remote restore |
| `asc cache push` | | Remote upload of queued keys |

The local store is opened directly with `asc_buildcache::Store::open`, at the path
`asc cache status --json` reports, so lookups and restores do not start a process per task.

### Task hash

For task `T` of project `P`:

```
key = ActionKey::builder("task")
        .part("def",      blake3(postcard(resolved task: argv, cwd, outputs, env names, shell)))
        .part("project",  P.key)
        .part("platform", "<os>-<arch>")
        .part("tasks",    FileId of .asc/tasks.toml, or empty)
        .part("env:NAME", value)            for each name in env, sorted; unset differs from ""
        .part("tool:NAME", output)          for each tool_version probe, sorted
        .part("deps",     sorted (dep task id, dep task key))
        .inputs(project input hash)
        .finish()
```

- **Project input hash.** For a clean work tree under the task's inputs this is exactly
  `input_hash(P, T, HEAD)` of SPEC §12A.6: trees only, memoised in `phash.v1`, never a file read.
  When some input paths are dirty, `asc inputs --project --target` (unit 2) computes the same
  formula over HEAD's tree with the dirty entries overlaid, hashing only the changed and untracked
  files (`cli/inputs.rs:364`). Clean subtrees still cost one tree id.
- **The tool name `task`** keeps task entries apart from `asc cache run` entries (`TOOL = "run"`,
  `cli/cache/run.rs:19`), since the first thing hashed is the tool (`key.rs:14`).
- **`deps`** includes the keys of the tasks `T` depends on, so a changed build output upstream
  changes downstream keys even when the downstream inputs did not change. `^` items in inputs
  already fold dependency inputs in; `deps` covers task-to-task edges inside one project too.
- **Tool versions.** `tool_version = { node = ["node", "--version"] }` in `[defaults]` runs each
  probe once per `asc run` and adds its trimmed stdout. Without a probe, a toolchain change does not
  change the key; `external_fp` already covers lockfile dependencies.
- No absolute path enters the key, so every worktree, workflow and machine of one platform shares
  entries, as with every other adapter (`key.rs:2`).

### Scheduling

1. Resolve the task set: the requested task names over the selected projects (`--select`,
   `--type`, or `--affected`), then the closure over `depends_on`.
2. Build the task graph. Order ties and SCCs follow `topo_order` (`affected.rs:632`): dependencies
   first, then display id. The order is deterministic, so two runs print tasks in the same order.
3. Key tasks in dependency order (a key needs its dependencies' keys). Keying needs no task to run.
4. Run a ready task as soon as its dependencies are done: look up locally, then remotely, then run.
   Parallelism is `-j <n>`, default the number of logical CPUs, and `--concurrency-per-task`
   weights (`weight = 4` for a task that is itself parallel, such as `cargo test`) so heavy tasks do
   not oversubscribe. Restores do not count against `-j`.
5. On failure: by default no new task starts, running ones finish, and the run exits 1.
   `--continue` runs every task whose dependencies succeeded.

`--dry-run` prints the plan and every key with `hit`, `remote` or `miss`, and runs nothing.

### Affected-only runs

`asc run test --affected [--base <rev>] [--uncommitted]` takes the projects from
`asc affected --json --hashes --target test` (SPEC §12A.7), so global inputs, eco-global files,
lockfile changes and `[global] unowned = "all"` behave exactly as in `asc affected` and in CI's
`asc ci affected`. The default base is the stack base (`crates/asc-ci/src/stack_base.rs`), which is
trunk for most work. The tasks of unaffected projects still run when an affected task depends on
them; they are usually cache hits.

### Remote cache

- **Mode.** `asc.buildcache.remote` (`off | read | read-write`) as for every adapter: unset is
  `read-write` when `CI` is set and `read` elsewhere (`docs/designs/build-cache.md` §3).
- **Restore.** Local miss and mode not `off`: the module collects every missing key of the ready
  tasks and calls `asc cache fetch` with up to 64 keys at once. The core fetches through the scope
  rules (caller's scope, then `trusted`), verifies the artefact and stores it locally; the module
  then restores from the local store. An unreachable server is a miss with one warning per run.
- **Upload.** In `read-write` mode each new entry and each hit is queued (`queue_push`). At the end
  of the run, the module starts `asc cache push` in the background (at most once a minute, as
  `spawn_push` does, `cli/cache/run.rs:125`). `asc run --push` pushes in the foreground; CI uses it.
- **What is stored.** Only exit status 0, as in `asc cache run`: the declared outputs that exist,
  up to 16 MiB of each of stdout and stderr (more: the task runs uncached and says so), the duration
  and the label `<project id>#<task>`.

### Log replay

A hit replays the stored stdout and stderr to the same streams, prefixed per task when more than
one task runs (`build rust:asc-core | `). `--output-logs=full|hits-only|errors-only|none` controls
what is printed, default `full` for misses and a one-line `cache hit` for hits.

Replayed bytes from an entry that was fetched from the server are printed through a filter that
removes terminal control sequences other than SGR colour (no OSC, no cursor movement), because the
bytes were written on another machine.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | Every task succeeded or was restored |
| 1 | At least one task failed; with one task, its own exit code when it is 1 to 125 |
| 2 | Configuration error: invalid `.asc/tasks.toml`, unknown task, task cycle, API mismatch |
| 129 | Usage error (the core's convention, `cli/common.rs:91`) |
| 130 | Interrupted; running tasks were stopped |

## Hermeticity and sandboxing

The runner is not hermetic. It promises what it does itself, and states the rest.

**Promised on every operating system:**

- The key covers the declared inputs by content, the resolved command, the `env` values, the
  probes, the platform and the dependency keys. Two runs with equal keys ran equal commands over
  equal declared inputs.
- The environment is scrubbed: the child gets `PATH`, `HOME`, the temporary-directory variables,
  the `env`, `pass_env` and `secret_env` variables, and `ASC_TASK`, `ASC_PROJECT`,
  `ASC_PROJECT_ROOT`. This is `asc_ci::sandbox`'s model (`sandbox.rs:10` to `:13`), moved to a
  shared crate in unit 5. Unlike CI workflow steps, `PATH` is the user's `PATH`, since tasks run
  local toolchains.
- No shell is added. The cwd must be inside the work tree. A timeout applies (default 30 min).
- After a miss the runner asks the work tree which paths changed during the task (asc's working
  copy already tracks this) and warns about every changed path that is not a declared output:
  `build rust:asc-core wrote target/x.json, not a declared output`. `--strict-outputs` makes that a
  failure and stores nothing.

**Not promised:** that a task reads only its declared inputs; that it has no network access; that
its toolchain is the one the key implies when no probe is declared; that its outputs are
reproducible. An undeclared input gives a stale hit; the fix is to declare it.

**Per operating system:**

| | Linux | macOS | Windows |
|---|---|---|---|
| Process tree stopped on timeout or Ctrl-C | process group | process group | job object (Q5) |
| File system isolation | opt-in `[sandbox] wrapper` (bwrap) | none | none |
| Network off | opt-in wrapper (`bwrap --unshare-net`) | none | none |
| Undeclared-output check | yes | yes | yes |

The wrapper is optional for `asc run` because the user runs their own repository's commands on
their own machine. `allow_unisolated` (`sandbox.rs:79`) is set for the runner; CI and agent tools
keep refusing unisolated commands as today.

## Security

- **Repository-defined commands.** `asc run` executes commands from the repository, as
  `npm run`, `cargo test` and `make` do. It runs only when a person or a configured agent invokes
  it: never on checkout, fetch, sync, a hook or a daemon event. The commands of the revision being
  run are the ones executed; `asc run --rev <other>` is refused, since it would execute another
  revision's commands against this tree.
- **Untrusted cache entries.** A restored entry writes files and prints bytes. Defences:
  output paths are checked when an entry is read (`check_paths`, `store.rs:253`) and a restore never
  writes through a link (`docs/designs/build-cache.md` §3); artefacts are verified blob by blob
  against their ids; replayed output from fetched entries is filtered (section "Log replay");
  reads take the caller's scope, then `trusted`, and never another developer's or another PR's
  scope.
- **Who may write the remote cache.** Unchanged from the build cache: `trusted` only from CI on the
  default branch; `pr-<n>` from that PR's CI; `dev-<user>` from a developer. A PR cannot place an
  entry that trunk builds read. `asc run --push` from a laptop writes `dev-<user>` only.
  Under path rules a `trusted` entry is served only where CI registered its directory and the
  caller may build there.
- **Environment in keys.** A key is a blake3 hash and keys are uploaded (`build-revs`), so a
  low-entropy secret in `env` could be guessed by hashing candidates. Secret-looking names are
  refused in `env` and `pass_env` with an error that names `secret_env`. `secret_env` variables are
  passed to the command, never enter the key (a secret should not change outputs), and their
  values are masked in captured output before it is stored or printed (`MASK`, `sandbox.rs:147`).
  A task whose captured output contained a masked value is stored with the masked text and flagged
  `masked` in its entry.
- **Environment in logs.** `--json` events and `asc run --explain` print variable names and
  whether a value is set, never values. The `env` values are shown only by
  `asc run --explain --show-env` on a terminal.
- **The config file.** `.asc/tasks.toml` is bounded on read (section "Configuration"). Selectors
  and globs are compiled with the limits of `asc-graph` (`config.rs:36`).
- **The module binary.** Installed only from a minisign-verified archive or a package manager. The
  core resolves it from `PATH`, then its own module directory, never from the work tree or the
  current directory (`find_in` over absolute `PATH` entries).

## CLI, JSON and MCP

### Commands

```
asc run <task>... [--select <sel>]... [--type <eco>] [--affected [--base <rev>] [--uncommitted]]
                  [-j <n>] [--continue] [--dry-run] [--no-cache] [--no-remote] [--push]
                  [--output-logs <mode>] [--strict-outputs] [--json] [-- <args for the task>]
asc run ls [--json]                 tasks per project, with their source
asc run graph [<task>] [--json]     the task graph, as text or JSON (nodes, edges)
asc run explain <project>#<task>    every part of the key with its value or hash, and the last entry
asc run init                        print a starter .asc/tasks.toml
asc run mcp                         MCP server over stdio
```

Arguments after `--` are passed to the named task of every selected project and enter the key.

### Output for people

One line per task when it starts and ends, prefixed by task and project, the task's own output
streamed under it, and a summary:

```
 hit   build  rust:asc-core           0.1 s
 run   build  node:@asc/ui            12.4 s
 fail  test   node:@asc/ui            3.2 s   exit 1
4 tasks: 1 run, 2 hit (1 remote), 1 failed; 15.8 s; 41.2 s saved
```

### Output for agents

`--json` writes one JSON object per line to stdout, and the tasks' output goes to stderr with the
task prefix. The schema is versioned (`"v": 1`) and documented in `docs/RUN.md`:

```json
{"v":1,"event":"plan","tasks":[{"id":"rust:asc-core#build","key":"4f1c...","cache":"hit"}]}
{"v":1,"event":"start","id":"node:@asc/ui#build"}
{"v":1,"event":"end","id":"node:@asc/ui#build","status":"ok","exit":0,"ms":12400,"cache":"miss"}
{"v":1,"event":"summary","ok":false,"run":1,"hit":2,"failed":["node:@asc/ui#test"],"ms":15800}
```

A failed task's `end` event carries the last 64 lines of its stderr, masked and stripped of
control sequences, so an agent can act without a second call.

### CI

```
asc ci checkout ...                       # existing
asc run build test --affected --base "$BASE" --push --json > run.jsonl
```

- The base is the plan job's base (`asc ci affected`); `--push` uploads in the foreground before
  the job ends.
- Matrix jobs: `asc affected --matrix-by project --format gha` splits projects across jobs as
  today; each job runs `asc run test --select <its projects>`.
- `run.jsonl` is the record the hub reads (section "Hub"), uploaded with `asc ci artifact upload`.

### MCP

`asc run mcp` is an MCP server over stdio, in the module (the core's `asc mcp` has no knowledge of
the module). Tools, with the same names and schema style as `crates/asc-mcp/tools.toml`:

| Tool | Write level | Does |
|---|---|---|
| `run_list_tasks` | read | `asc run ls --json` |
| `run_plan` | read | `--dry-run --json`: tasks, keys, hit or miss |
| `run_explain` | read | the key parts of one task |
| `run_tasks` | execute | runs tasks after confirmation (`crates/asc-mcp/src/confirm.rs`) |

The core's `asc mcp` may list the module's server in its `resources` when `asc-run` is installed
(open question 6).

## Hub

The hub shows only what the server already stores; the runner adds no new server route in this
design.

- **Per revision: the tasks built.** `POST /v8/asc/build-revs/<rev>` (`turbo.rs:1072`) already
  records every key a build of a revision used, per scope. Entries with tool `task` carry the
  label `<project id>#<task>`, the outputs and their sizes, the creation time and the origin
  workflow (`Action`, `store.rs:52`). The changeset page lists them: project, task, outputs size,
  and whether CI on trunk (`trusted`) has the same key.
- **Per project: task coverage.** From the same lists and the project graph the server already
  computes: which projects have a cached `build`/`test` entry for trunk's head.
- **Affected.** The PR page already shows `asc affected` reasons; it adds the task names that would
  run for them, from `.asc/tasks.toml` at the PR head (parsed with the module's library, which the
  server may link; it has no dependency on the module binary).
- **CI results.** `run.jsonl`, uploaded as a CI artefact, gives per-task status and duration. The
  CI insights store (`crates/asc-serve/src/ci/insights/`) takes each failed task's output through
  `normalize` and `flaky::analyse` like any test result, so a flaky task is reported the same way
  as a flaky test.
- **Cache use.** Hit ratio per workflow from the artefact store's own access counts, where it keeps
  them; otherwise from `summary` events in `run.jsonl`.

## Implementation units

Each unit is one bounded change with its tests and docs. Units 1 to 4 change the core and are
useful without the module; the core never depends on `asc-run`.

1. **Module discovery and `asc module list|info`.** Files: `crates/asc/src/dispatch.rs` (module
   directory in `external`, the `run` hint, `ASC_EXE` in `run_external`, `module` in
   `ASC_ONLY_VERBS`), `crates/asc/src/cli/module.rs` (new), `crates/asc/src/cli/mod.rs` (entry).
   Tests: resolution order with a fake module directory, `run` stays external, missing-module
   message, `info --json` shape. Docs: `docs/CLI.md` (`asc module`), SPEC §0 out-of-scope entry
   changed to "`asc run` task execution in the core (an optional module,
   docs/design/TASK-RUNNER.md)".
   Acceptance: `cargo test -q -j 4 -p asc module`.
2. **Project input hash with the dirty overlay.** Files: `crates/asc/src/cli/inputs.rs`
   (`--project`, `--target`), `crates/asc-engine/src/projects/hash.rs` (a variant of
   `compute_hashes` over an overlaid tree). Tests: clean tree equals `input_hashes` for HEAD;
   one dirty file changes only its project's hash; an ignored file changes nothing; no clean file is
   read (counting reader). Docs: SPEC §12A.6 note, `docs/CLI.md`. Acceptance:
   `cargo test -q -j 4 -p asc-engine input_hash`.
3. **`asc cache fetch <key>...`.** Files: `crates/asc/src/cli/cache.rs` (subcommand),
   `crates/asc/src/cli/cache/remote.rs` (batch over `fetch`, eight at a time). Tests: fetched,
   absent and refused keys reported per key in JSON; mode `off` refused; an unreachable server is a
   warning and exit 0. Docs: `docs/designs/build-cache.md` §5. Acceptance:
   `cargo test -q -j 4 -p asc cache_fetch`.
4. **`asc module install|remove`, upgrade note and doctor check.** Files:
   `crates/asc/src/cli/module.rs`, `crates/asc/src/cli/upgrade.rs` (share `verify`, `unpack`,
   `prune` with the module path), `crates/asc/src/cli/doctor.rs` (`check_modules`). Tests: install
   from `--from <dir>` with a signed fixture, bad signature refused before unpack, prune keeps the
   active version, doctor lines for absent, ok, API mismatch and shadowed copies. Docs:
   `docs/CLI.md`, `docs/RELEASING.md` (module archives). Acceptance:
   `cargo test -q -j 4 -p asc module_install`.
5. **Shared sandbox crate.** Move `crates/asc-ci/src/sandbox.rs` to `crates/asc-sandbox/src/lib.rs`
   with no behaviour change; `asc-ci` re-exports it as `asc_ci::sandbox`. Add an
   `inherit_path` option for local runs. Tests: the existing sandbox tests move with it; a test that
   `inherit_path` keeps the caller's `PATH` and nothing else. Docs: crate docs,
   `docs/DEPENDENCIES.md`.
   Acceptance: `cargo test -q -j 4 -p asc-sandbox sandbox`.
6. **`asc-run` crate, core client and layering test.** Files: `crates/asc-run/Cargo.toml`,
   `src/main.rs`, `src/lib.rs`, `src/core.rs` (spawn `$ASC_EXE`, parse the JSON of module API 1,
   version check), `tests/layering.rs`, root `Cargo.toml` member. Tests: API range accepted and
   refused with the message; JSON parsing of each core call from recorded fixtures; no crate names
   `asc-run`. Docs: `crates/asc-run/README.md`. Acceptance: `cargo test -q -j 4 -p asc-run core`.
7. **Task configuration.** Files: `crates/asc-run/src/config.rs` (`.asc/tasks.toml`),
   `src/sources/node.rs`, `src/sources/cargo.rs`, `src/sources/argv.rs` (A4 table through
   `commands_for`). Tests: merge order, bounds, secret-looking names refused, tracked outputs
   refused, package manager from `packageManager`. Docs: `docs/RUN.md` (configuration).
   Acceptance: `cargo test -q -j 4 -p asc-run config`.
8. **Task graph and plan.** Files: `crates/asc-run/src/plan.rs`. Tests: `^task` over all edge kinds,
   missing dependency task skipped, task cycle named, project SCC handled, deterministic order equal
   to `topo_order`. Docs: `docs/RUN.md` (dependencies). Acceptance:
   `cargo test -q -j 4 -p asc-run plan`.
9. **Task key.** Files: `crates/asc-run/src/key.rs`. Tests: each part changes the key; unset and
   empty variables differ; `secret_env` never changes it; no absolute path in the material
   (two worktrees, one key); dependency key propagation. Docs: `docs/RUN.md` (keys).
   Acceptance: `cargo test -q -j 4 -p asc-run key`.
10. **Executor.** Files: `crates/asc-run/src/exec.rs` (ready queue, `-j`, weights, `--continue`,
    timeout, Ctrl-C, undeclared-output check). Tests: parallel limit respected with weights,
    stop-on-failure, `--continue`, timeout kills the process tree on Unix, undeclared output warned
    and `--strict-outputs` fails. Docs: `docs/RUN.md` (execution). Acceptance:
    `cargo test -q -j 4 -p asc-run exec`.
11. **Cache integration.** Files: `crates/asc-run/src/cache.rs` (lookup, `sync` restore,
    `put_outputs`, `put_action`, `queue_push`, batched `asc cache fetch`, background and `--push`
    push). Tests: hit restores outputs and replays logs with the stored exit status; failure not
    stored; 16 MiB stream cap runs uncached; evicted blob is a miss; fetched entry with an escaping
    path refused. Docs: `docs/RUN.md` (cache). Acceptance: `cargo test -q -j 4 -p asc-run cache`.
12. **Output and exit codes.** Files: `crates/asc-run/src/report.rs` (human lines, JSON events,
    control-sequence filter, masking). Tests: event schema snapshot, exit code table, OSC sequences
    stripped from fetched logs, masked values never printed. Docs: `docs/RUN.md` (JSON schema).
    Acceptance: `cargo test -q -j 4 -p asc-run report`.
13. **Affected runs.** Files: `crates/asc-run/src/affected.rs`. Tests: projects from recorded
    `asc affected --json --hashes` output; global input change runs everything; dependency tasks of
    affected projects included. Docs: `docs/RUN.md` (affected), `docs/ci/` example. Acceptance:
    `cargo test -q -j 4 -p asc-run affected`.
14. **MCP server.** Files: `crates/asc-run/src/mcp.rs`, `crates/asc-run/tools.toml`. Tests:
    deterministic `tools/list`; `run_tasks` refused without confirmation; read tools return the
    same JSON as the CLI. Docs: `docs/mcp.md` (module section). Acceptance:
    `cargo test -q -j 4 -p asc-run mcp`.
15. **Packaging.** Files: `packaging/homebrew/asc-run.rb.in`, `packaging/scoop/asc-run.json.in`,
    `packaging/winget/asc.asc-run.yaml` and its installer and locale files,
    `packaging/aur/PKGBUILD-asc-run.in`, `packaging/linux/nfpm-asc-run-amd64.yaml.in` and `-arm64`.
    Tests: `crates/asc-run/tests/packaging.rs` checks each template names `asc-run`, depends on the
    core package and uses the version placeholder. Docs: `docs/RELEASING.md`. Acceptance:
    `cargo test -q -j 4 -p asc-run packaging`.
16. **Turbo parity.** Files: `tests/turbo/tests/run_parity.rs`, `tests/turbo/src/lib.rs` (helper).
    Runs `asc-run build` and turbo 2.10.11 over `tests/turbo/fixture` (`dependsOn: ["^build"]`,
    `outputs: ["dist/**"]`) and compares which packages build, in which order, and that a second
    run is all hits. Skips with a message when turbo is not installed, like the contract suite.
    Docs: SPEC §17 note. Acceptance: `cargo test -q -j 4 -p asc-turbo-contract run_parity`.
17. **Hub: tasks per revision.** Files: `crates/asc-serve/src/turbo.rs` (a `tool=task` filter on
    `GET /v8/asc/build-revs/<rev>` that returns labels and sizes from the stored entries).
    Tests: filter returns only task entries of the caller's scope and `trusted`; another PR's scope
    never appears. Docs: `docs/api.md`. Acceptance: `cargo test -q -j 4 -p asc-serve build_revs`.

## Open questions

1. **Task file name.** The brief said an `asc.toml` section; the repository keeps configuration in
   `.asc/` and the build-cache design records that there is no top-level `asc.toml`.
   Recommended: `.asc/tasks.toml`, as designed here, and no `[tasks]` table in
   `.asc/projects.toml`, because a change to `projects.toml` marks every project affected.
2. **Does the core MCP server forward to the module?** Recommended: no for the first release;
   `asc run mcp` is its own server. Revisit when agents ask for one server.
3. **Import `turbo.json`?** Recommended: yes, read-only: `asc run init --from-turbo` converts
   `dependsOn`, `outputs`, `inputs` and `env` into `.asc/tasks.toml` once; the runner never reads
   `turbo.json` at run time, so there is one source of truth.
4. **Tool version probes by default?** Recommended: no default probes; `asc run init` writes
   `node --version` and `rustc --version` probes when it finds those ecosystems, so the user sees
   and owns them.
5. **Windows process trees.** A job object needs Win32 calls, and the workspace denies
   `unsafe_code`. Recommended: use a maintained crate that wraps job objects (reviewed under
   `docs/DEPENDENCIES.md`), confined to `asc-sandbox` behind `cfg(windows)`; until then, stop the
   direct child and report that grandchildren may survive.
6. **Core `asc mcp` listing the module.** Recommended: yes, as one resource entry naming the
   `asc run mcp` command when `asc-run` is installed, with no code from the module.
7. **Store a failed run's logs?** Recommended: no. Only exit 0 is stored, as in `asc cache run`;
   failures are cheap to rerun and storing them would replay a failure caused by a flaky resource.
8. **Remote-only execution (running tasks on the server).** Recommended: out of scope. It needs
   isolation the runner does not promise; CI already runs commands in its sandbox.
9. **Module release cadence.** Recommended: the module has its own version number and release; the
   module API integer, not the core version, decides compatibility, so a core patch release never
   requires a module release.
