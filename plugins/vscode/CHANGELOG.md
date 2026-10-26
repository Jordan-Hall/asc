# Changelog

All notable changes to the ASC extension are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Security

- The extension starts only an absolute `asc` path outside the workspace folders, from the executable's own directory, with the root passed as `-C`; a planted `asc.exe` is never run.
- A stored token is injected only when every root has a workspace id and an https host equal to the token's host; `ASC_TOKEN_HOST` names it and `asc` refuses to send `ASC_TOKEN` elsewhere. The check runs on every start, and the bridge restarts only when the injected environment changed.

### Added

- Release workflow (`vscode-release.yml`): `vsce` and `ovsx` publish on a `vscode-v<version>` tag, with `scripts/release-check.ts` as the dry run. The first publish is to be verified by hand and recorded here.
- Packaging as a VSIX with `vsce package` (README, CHANGELOG and icon).
- Detection of asc workspaces, lazy start of the `asc` bridge, and the built-in git prompt.
