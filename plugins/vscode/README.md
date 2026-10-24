# ASC for VS Code

Source control for [asc](https://forge.example.org/asc/source-control) workspaces. The
extension is inert in folders that are not asc workspaces: it registers nothing and starts no
process.

## Requirements

- VS Code 1.101 or later (also VSCodium and Cursor builds based on that engine).
- The `asc` executable. The extension looks at the `asc.path` setting, then `PATH`, then
  `$ASC_HOME/bin`. The `asc` release must report the `plugin.v1` and `json-verbs.v1` features;
  otherwise the extension shows "Update asc" and disables commands that change the repository.

## Features

- Activation only when the workspace contains `.git`, uses the `asc:` file system, or is opened
  through a URI. It never activates on `*`.
- One notification offers to turn off the built-in git (`git.enabled`) for an asc workspace.
- Commands: `asc: Restart the bridge` and `asc: Show output`.

## Security

- `asc` starts only in trusted workspaces. The `asc.path` setting is restricted in untrusted
  workspaces and is machine-scoped.
- Connections to the forge use https or ssh. Tokens are held in VS Code SecretStorage and never
  appear in arguments or logs.
- Virtual workspaces are not supported.

## Settings

| Setting | Default | Meaning |
|---|---|---|
| `asc.path` | empty | Path to the `asc` executable. |

## Install

Install the `.vsix` file with `code --install-extension asc-<version>.vsix`, or use
"Extensions: Install from VSIX...".
