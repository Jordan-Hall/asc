# asc documentation

Start with [`GETTING-STARTED.md`](GETTING-STARTED.md). Use [`CLI.md`](CLI.md) to find a command and
[`CONFIG.md`](CONFIG.md) to find a configuration key. When two pages disagree, [`SPEC.md`](SPEC.md)
wins.

## Use asc

| Page | What is in it |
|---|---|
| [`GETTING-STARTED.md`](GETTING-STARTED.md) | Install, sign in, clone, the daily loop, git compatibility. |
| [`CLI.md`](CLI.md) | Every command, with an example you can run. |
| [`CONFIG.md`](CONFIG.md) | Every `asc.*` key and environment variable, with its default and an example. |
| [`CHEATSHEET.md`](CHEATSHEET.md) | Common tasks and the asc command for each. |
| [`ONBOARDING.md`](ONBOARDING.md) | `asc doctor`, editor settings, the optional `git` shim, and fixing what `asc doctor` reports. |
| [`mcp.md`](mcp.md) | `asc mcp`: the tools an agent can call. Generated from the code. |
| [`api.md`](api.md) | The `asc.v1` RPC services and how to call them with `asc api`. |
| [`BYOS.md`](BYOS.md) | Keeping objects in a bucket you own. |
| [`agents/claude-code-worktrees.md`](agents/claude-code-worktrees.md) | Running Claude Code in asc worktrees. |

## Editor plugins and the web app

| Page | What is in it |
|---|---|
| [`plugin-api.md`](plugin-api.md), [`plugin-api-stack.md`](plugin-api-stack.md), [`plugin-api-additions.md`](plugin-api-additions.md) | The Workspace RPCs the VS Code and JetBrains plugins use. |
| [`WEB-UI.md`](WEB-UI.md) | The web app: routes, what each screen reads, keyboard, security headers. |
| [`CI-INSIGHTS.md`](CI-INSIGHTS.md) | CI failure groups, flaky tests and quarantine. |

## Run a server

| Page | What is in it |
|---|---|
| [`../deploy/README.md`](../deploy/README.md) | Deploying `asc serve`. |
| [`RELEASE.md`](RELEASE.md), [`RELEASING.md`](RELEASING.md) | How releases are built, signed, published and installed. |
| [`metrics.md`](metrics.md) | The metrics every tier exports. |
| [`AUDIT.md`](AUDIT.md) | The hash-chained audit log and ref undo. |
| [`runbooks/`](runbooks) | Cutover, restore, rollback, and one runbook per tier. |
| [`garage-compat.md`](garage-compat.md) | Measured results against Garage. |

## How asc works and how it is checked

| Page | What is in it |
|---|---|
| [`SPEC.md`](SPEC.md) | The authoritative specification. |
| [`PLAN.md`](PLAN.md), [`PLAN-STATUS.md`](PLAN-STATUS.md), [`STATUS.md`](STATUS.md), [`CARRY.md`](CARRY.md) | The implementation plan, which units are done, the honest state, and obligations carried forward. |
| [`BUILDOUT.md`](BUILDOUT.md) | The contract for finishing the features ASC Hub lists as partial. |
| [`git-fidelity.md`](git-fidelity.md), [`git-quirks.md`](git-quirks.md), [`git-defects.md`](git-defects.md), [`divergences.md`](divergences.md), [`adapter-gaps.md`](adapter-gaps.md) | How faithful asc is to git, and every place it is not. |
| [`BENCHMARKS.md`](BENCHMARKS.md) | asc compared with git. |
| [`SCION-TRACEABILITY.md`](SCION-TRACEABILITY.md) | The Scion checklist mapped to the spec. |
| [`designs/`](designs), [`research/`](research), [`DECISIONS/`](DECISIONS) | Design notes, research and decision records. |
| [`RENAME.md`](RENAME.md), [`brand/BRAND.md`](brand/BRAND.md) | Naming and brand. |
