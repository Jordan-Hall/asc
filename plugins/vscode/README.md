# ASC for VS Code

Source control for [asc](https://forge.example.org/asc/source-control) workspaces. The
extension is inert in folders that are not asc workspaces: it registers nothing and starts no
process.

## Requirements

- VS Code 1.101 or later (also VSCodium and Cursor builds based on that engine).
- The `asc` executable. The extension looks at the `asc.path` setting (an absolute path), then the
  absolute entries of `PATH`, then `$ASC_HOME/bin`. It never runs a file from a workspace folder or a
  name searched in the current directory. The `asc` release must report the `plugin.v1` and `json-verbs.v1` features;
  otherwise the extension shows "Update asc" and disables commands that change the repository.

## Features

- Activation only when the workspace contains `.git`, uses the `asc:` file system, or is opened
  through a URI. It never activates on `*`.
- One notification offers to turn off the built-in git (`git.enabled`) for an asc workspace.
- Commands: `asc: Restart the bridge` and `asc: Show output`.
- Build cache: a status item with the hits and misses of the last build (and a warning when the
  build cache server cannot be reached), a Build cache view of the entries by tool with Show and
  Delete, `asc-build` tasks for Cargo, Go and the package.json scripts that run Rsbuild, Rslib or
  Vite, and the commands Set up build caching, Warm, Clean up and Clear. A task in tasks.json with
  `name`, `inputs`, `outputs` and `command` runs through `asc cache run`:

  ```json
  { "type": "asc-build", "name": "docs", "inputs": ["docs"], "outputs": ["site"], "command": ["mkdocs", "build"] }
  ```

## Security

- `asc` starts only in trusted workspaces. The `asc.path` setting is restricted in untrusted
  workspaces and is machine-scoped.
- Connections to the forge use https or ssh. Tokens are held in VS Code SecretStorage and never
  appear in arguments or logs. A stored token is passed to `asc` only when every open root has an https
  remote on the host it was given for, and `asc` sends it to that host only (`ASC_TOKEN_HOST`).
- Virtual workspaces are not supported.

## Settings

| Setting | Default | Meaning |
|---|---|---|
| `asc.path` | empty | Absolute path to the `asc` executable. |
| `asc.cache.terminalEnv` | `true` | Give new terminals the variables of `asc cache env` (turbo, Nx, Maven). |

## Install

Install the `.vsix` file with `code --install-extension asc-<version>.vsix`, or use
"Extensions: Install from VSIX...".
