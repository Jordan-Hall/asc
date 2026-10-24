# Changelog

All notable changes to the ASC extension are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- Release workflow (`vscode-release.yml`): `vsce` and `ovsx` publish on a `vscode-v<version>` tag, with `scripts/release-check.ts` as the dry run. The first publish is to be verified by hand and recorded here.
- Packaging as a VSIX with `vsce package` (README, CHANGELOG and icon).
- Detection of asc workspaces, lazy start of the `asc` bridge, and the built-in git prompt.
