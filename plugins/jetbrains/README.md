# ASC for JetBrains IDEs

Source control for asc workspaces in IntelliJ IDEA and the other JetBrains IDEs. The plugin is
inert in projects that are not asc workspaces: its menus, tool window and status item stay hidden.

## Requirements

- A JetBrains IDE on build 261 (2026.1) or later.
- The `asc` executable. The plugin takes the first absolute `asc` it finds on `PATH`, then
  `$ASC_HOME/bin` (or `~/.asc/bin`). It never runs a file inside an open project, and it starts `asc`
  from the executable's own directory with `-C <root>`.

## Features

- **Version control.** Each asc root is mapped to the `asc` VCS (replacing a Git mapping). The
  Commit tool window lists changes, creates a change from the selected files or amends the current
  one, and offers Submit as a commit executor. Annotate, file history, diff, the merge dialog,
  rollback and `.ascignore` work through the IDE's own views.
- **The asc tool window** (bottom). Tabs Stack, Graph, Review, Inbox, Actions, Affected, Worktrees,
  Search and History, drawn by the shared `asc-ui` in the embedded browser. The browser starts on
  first open and is released after ten minutes hidden. Without the embedded browser, the window links
  to the same view on the forge. **View > asc Views** (also under VCS) opens the window on any tab.
- **The asc menu** (VCS > asc):
  - Stack: Move Up, Move Down, Go to the Top, Go to the Bottom, Restack, Sync, Submit, Absorb
    (shows what would move, then asks), Undo, Redo, Uncommit, Unamend, Continue or Abort a paused
    operation.
  - Remote: Fetch, Pull, Push, Share the Current Change, Get a Stack into a Worktree (a revision,
    then a folder inside the root).
  - Tags: Show Tags, Create Tag (a message makes it annotated), Delete Tag (pick one, then
    confirm), Push All Tags.
  - Stash: Stash Changes (optionally with untracked files), Pop, Apply, Drop.
  - Configure Agents: writes the project's MCP configuration with `asc mcp install --scope project`.
  - Sign In to a Host, Sign Out of a Host, Show Sign-in Status.

  Submit, Push, Push All Tags, Delete Tag and Drop Stash ask first; the shared allowlist decides which commands do.
- **Status bar.** The current change, its place in the stack, how far the stack is behind trunk and
  its review state. A click restacks when the stack needs it, otherwise it opens the tool window.
- **Path rules.** Files the repository's path rules keep read-only for you cannot be edited.
- **Large workspaces.** Generated folders are excluded from indexing; a notice appears when
  indexing would download virtual files.
- **TSRX.** `.tsrx` files are coloured with the same TextMate grammar as the VS Code extension.

## Sign-in

Sign In asks for the forge host (the hosts of the project's https remotes are offered first), then
for the browser (OAuth with PKCE, answered on a loopback port) or a pasted access token. The token
goes to your credential helper through `asc`. Without a helper it is kept in the IDE password store,
and it is passed to `asc` only when every root's remote is on that host, together with the host
(`ASC_TOKEN_HOST`), so `asc` sends it nowhere else. Connections use https only. Tokens never appear
in arguments, settings or logs.

## Install

Settings > Plugins > the gear menu > **Install Plugin from Disk...**, and choose
`asc-jetbrains-<version>.zip` from the release.

## Development

```sh
./gradlew test                  # unit tests
./gradlew buildPlugin           # build/distributions/*.zip
./gradlew verifyPluginStructure # plugin.xml and the archive layout
./gradlew uiTest                # IDE Starter UI tests (downloads an IDE)
```

Build `packages/asc-ui` first (`bun run build`) or the tool window pages are empty.
