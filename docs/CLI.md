# asc command reference

Every `asc` command with one example you can run. The table of contents is the list of groups. Two
rules cover the whole page:

- The everyday commands (`clone`, `status`, `commit`, `log`, `push` ...) have stable flags, output,
  exit codes, `--porcelain`, `-z` and `--format` (see [`CHEATSHEET.md`](CHEATSHEET.md)). Its row here
  shows one common use; `asc help <command>` shows asc's notes and then the compatibility manual.
- The other commands (the stack verbs, `agent`, `ci`, `workflow`, `search`, `serve` ...)
  is described by its own flags below. Most of them take `--json`, which prints one JSON object
  (or JSON lines for streams) instead of text. The `--remote <name|url>` flag, where a command has
  it, picks the server; the default is `origin`.

Configuration keys are in [`CONFIG.md`](CONFIG.md). Getting started is in
[`GETTING-STARTED.md`](GETTING-STARTED.md). The index of all docs is [`README.md`](README.md).

1. [Set up and look after an install](#1-set-up-and-look-after-an-install)
2. [The working copy](#2-the-working-copy)
3. [Remotes](#3-remotes)
4. [History and search](#4-history-and-search)
5. [Merging and patches](#5-merging-and-patches)
6. [Stacks of changes](#6-stacks-of-changes)
7. [Code intelligence](#7-code-intelligence)
8. [Projects and CI](#8-projects-and-ci)
9. [Agents and workflows](#9-agents-and-workflows)
10. [Server and API](#10-server-and-api)
11. [Plumbing](#11-plumbing)

## 1. Set up and look after an install

| Command | What it does | Example |
|---|---|---|
| `init` | Creates a repository. `--store <url>` keeps objects in a bucket; `--refs local\|shared` says who holds the bucket's refs. | `asc init -b master my-project` |
| `config` | Reads and writes git config, with git's flags. asc's own keys are in [`CONFIG.md`](CONFIG.md). | `asc config --global asc.live.update true` |
| `clone` | Clones lazily: file contents download when first read. `--mode plain` writes real files. | `asc clone https://asc.example.org/acme/payments` |
| `auth status` | Names the source that would supply a credential for a URL, never the secret. | `asc auth status https://asc.example.org` |
| `auth login` | Stores a token through the system credential helpers. The token is read from standard input, never from an argument. | `printf '%s' "$TOKEN" \| asc auth login --host https://asc.example.org --token-stdin` |
| `auth logout` | Forgets the stored token for a host. | `asc auth logout --host https://asc.example.org` |
| `doctor` | Checks the install, git and the store. `--fix` repairs what needs no administrator rights; `--workflows` lists push triggers on non-shared branches. | `asc doctor --json` |
| `install-shim` | Optional git compatibility: puts a `git` next to `asc` so `git` is asc inside an asc workspace and real git elsewhere. | `asc install-shim` |
| `upgrade` | Signature-verified upgrade, side by side. The release source is `asc.releasesUrl`, `ASC_DOWNLOAD_BASE_URL` or `--from <dir>`. | `asc upgrade --check` |
| `self-update` | The same as `upgrade`, under the name the installers use. | `asc self-update --check` |
| `help` | asc's notes for a command, then git's manual; `asc help git` prints the compatibility command map. | `asc help git` |
| `daemon start`, `stop`, `status` | Starts, stops or reports the background daemon (uploads, live updates, the virtual file system). | `asc daemon status` |
| `debug stats` | The size of the cache and the store and the bytes each workspace holds. | `asc debug stats --json` |
| `debug hydrations` | Which processes made files download. `--by-process` groups them. | `asc debug hydrations --by-process` |
| `debug trace` | Asks the running daemon for a diagnostic dump in its log directory and prints the path. | `asc debug trace` |
| `gc` | Collects store garbage. `--dry-run` only reports; `--store <url>` collects a bucket. | `asc gc --dry-run` |
| `prefetch` | Downloads the files under a pathspec now, and `--history <n>` more history. | `asc prefetch src docs --history 500` |
| `sparse-checkout`, `sparse` | Chooses the directories in the cone: `set`, `add`, `list`, `disable`. `sparse dehydrate` re-virtualises clean files. | `asc sparse set crates/asc docs` |
| `import` | Imports git history into a repository. | `asc import --git ../old-repo --incremental` |
| `export` | Writes the repository as git objects into a directory. | `asc export --git ../git-copy --refs 'refs/heads/*'` |
| `migrate ignore` | Copies `.gitignore` files to `.ascignore`; `--move` removes the originals. | `asc migrate ignore --move` |
| `migrate branchless` | Moves a repository to branchless mode; `--prune-merged` also removes merged branches. | `asc migrate branchless --prune-merged` |
| `internal claude-hook` | Reads a Claude Code worktree hook from standard input; see [`agents/claude-code-worktrees.md`](agents/claude-code-worktrees.md). | `asc internal claude-hook worktree-create < hook.json` |

## 2. The working copy

| Command | What it does | Example |
|---|---|---|
| `status` | Working copy state. | `asc status --short` |
| `add` | Stages paths. `-p` chooses hunks. | `asc add -p src/lib.rs` |
| `rm` | Removes paths from the index and the tree. | `asc rm old.txt` |
| `mv` | Moves a path. | `asc mv a.txt b.txt` |
| `restore` | Restores paths from the index or a revision. | `asc restore --source=HEAD~1 -- README.md` |
| `reset` | Moves HEAD and the index. | `asc reset --soft HEAD~1` |
| `commit` | Records a commit (one cheap local row in a branchless repository). | `asc commit -m "add parser"` |
| `switch` | Switches branch. | `asc switch -c topic` |
| `checkout` | Switches branch or restores paths. | `asc checkout master` |
| `branch` | Lists, creates and deletes branches. In a branchless repository a branch is a local label. | `asc branch -vv` |
| `tag` | Lists and creates tags. | `asc tag -a v1.2.0 -m "release 1.2.0"` |
| `clean` | Removes untracked files. | `asc clean -n -d` |
| `stash` | `push`, `pop`, `apply`, `list`, `show`, `drop`, `clear`, `branch`, `create`, `store`. | `asc stash push -u -m "wip"` |
| `worktree` | `add`, `list`, `remove`, `prune`, `lock`, `unlock`, `move`, `convert --plain`. `add --ephemeral --ttl 7d` makes a throwaway one. | `asc worktree add ../review-1 topic` |
| `sparse-checkout` | See section 1. | `asc sparse-checkout list` |
| `check-ignore` | Says which ignore rule matches a path. | `asc check-ignore -v target/debug` |
| `check-attr` | Shows attributes of a path. | `asc check-attr -a src/lib.rs` |

## 3. Remotes

| Command | What it does | Example |
|---|---|---|
| `fetch` | Downloads refs and missing objects. | `asc fetch origin` |
| `pull` | Fetches and integrates. | `asc pull --ff-only` |
| `push` | Pushes refs. In a branchless repository `asc push origin feat` also submits the stack as draft changes. `--land` or `-o land` asks the server to land onto the default branch. | `asc push --land` |
| `remote` | Lists, adds and removes remotes. | `asc remote add origin https://asc.example.org/acme/payments` |
| `ls-remote` | Lists refs of a remote. | `asc ls-remote --heads origin` |
| `submodule` | Submodules are composed read-only from the nested repository. | `asc submodule status` |

## 4. History and search

| Command | What it does | Example |
|---|---|---|
| `log` | History. | `asc log --oneline -n 20` |
| `show` | Shows an object. | `asc show HEAD` |
| `diff` | Compares revisions, the index and the tree. | `asc diff --stat master...HEAD` |
| `blame` | Last change per line. | `asc blame -L 10,20 src/lib.rs` |
| `grep` | Searches tracked files. | `asc grep -n "TODO" -- src` |
| `rev-parse` | Resolves names. | `asc rev-parse HEAD` |
| `rev-list` | Lists commits. | `asc rev-list --count master..HEAD` |
| `merge-base` | Finds a common ancestor. | `asc merge-base master HEAD` |
| `describe` | Names a commit from tags. | `asc describe --tags` |
| `reflog` | The ref log. | `asc reflog -n 10` |
| `shortlog` | Commits per author. | `asc shortlog -sn` |
| `name-rev` | Names a commit from refs. | `asc name-rev HEAD` |
| `whatchanged` | Log with raw diffs. | `asc whatchanged -n 3` |
| `range-diff` | Compares two commit ranges. | `asc range-diff master topic@{1} topic` |
| `cherry` | Finds commits not applied upstream. | `asc cherry -v master` |
| `notes` | `list`, `show`, `add`, `get-ref`. | `asc notes list` |
| `archive` | Writes a tar or zip of a tree. | `asc archive --format=tar -o src.tar HEAD` |
| `bisect` | Binary search for a bad commit. | `asc bisect start HEAD v1.0.0` |
| `fsck` | Checks the object store. | `asc fsck` |
| `count-objects` | Counts objects. | `asc count-objects -v` |
| `search` | Index-backed code search, see section 7. | `asc search "retry_policy"` |

## 5. Merging and patches

| Command | What it does | Example |
|---|---|---|
| `merge` | Joins histories. | `asc merge topic` |
| `rebase` | Replays commits. `-i` opens the todo list. | `asc rebase --onto master topic~3 topic` |
| `cherry-pick` | Applies commits. | `asc cherry-pick abc1234` |
| `revert` | Reverts commits. | `asc revert HEAD` |
| `apply` | Applies a patch to the tree. | `asc apply fix.patch` |
| `am` | Applies a mailbox of patches. | `asc am 0001-fix.patch` |
| `format-patch` | Writes patches. | `asc format-patch -1 HEAD` |
| `merge-tree` | Merges without touching the tree. | `asc merge-tree --write-tree master topic` |
| `interpret-trailers` | Edits message trailers. | `printf 'subject\n' \| asc interpret-trailers --trailer 'Risk: low'` |

## 6. Stacks of changes

A stack is a chain of small changes, each with a stable change id. These verbs exist only in asc.
Read-only verbs take `--json`; verbs that change the repository take `-n` or `--dry-run` where it
makes sense. `asc undo` reverses the last operation; `asc op log` lists them.

| Command | What it does | Example |
|---|---|---|
| `create` | Makes a change on HEAD from the staged changes. `-a` stages tracked files first, `-p` chooses hunks, `--insert` reparents HEAD's children, `--onto <rev>` creates elsewhere. | `asc create -m "add parser"` |
| `modify` | Amends the current change; children are restacked. `-c` adds a new change instead, `--into <change>` amends a draft below HEAD. | `asc modify -a` |
| `absorb` | Folds staged hunks into the changes that own those lines. `-f` applies, `-n` only shows. | `asc absorb -f` |
| `split` | Splits a change: `--by-file <pathspec>...`, `--by-hunk`, `--by-commit` or `--hunks <spec.json>`. | `asc split --change HEAD --by-file src/a.rs -m "move a"` |
| `squash` | Squashes into the parent. | `asc squash -m "one change"` |
| `fold` | Folds a change into its parent. `--stack` folds everything above too, `--keep` keeps its message. | `asc fold --keep` |
| `reword` | Edits a message. | `asc reword -m "better message"` |
| `move` | Moves a change onto another parent. `--only` leaves the changes above in place. | `asc move --onto master --source HEAD` |
| `reorder` | Opens your sequence editor on the list of changes, one `<change> <subject>` per line, and rebuilds the stack in the order you save. Every change must stay in the list exactly once; use `abandon` to drop one. `-n` predicts conflicts, `--stack` includes the changes above HEAD. | `GIT_SEQUENCE_EDITOR=true asc reorder -n` |
| `restack` | Rebases a change and what is above it onto its parent. `--upstack`, `--downstack`, `--only`. | `asc restack --upstack` |
| `abandon` | Drops changes (the ones above stay). | `asc abandon -n` |
| `uncommit` | Moves the change's content back to the working copy. | `asc uncommit src/lib.rs` |
| `unamend` | Undoes the last amend. | `asc unamend -n` |
| `hide`, `unhide` | Hides a draft change from the stack views, or brings it back. | `asc hide abcd` |
| `sl`, `smartlog` | Shows the stack. `-s` only the current stack, `-a` landed too, `--remote` review data, `--cloud` cloud workspaces. | `asc sl -s` |
| `up`, `next`, `down`, `prev`, `top`, `bottom` | Moves HEAD along the stack. At a fork, `--to <change or branch>` picks. | `asc up 2` |
| `submit` | Sends the stack to the server as draft changes. `-s` the whole stack, `--publish` makes them reviewable, `--reviewer`, `--auto-merge`, `--update-only`. | `asc submit -s --reviewer ada` |
| `sync` | Fetches, fast-forwards trunk, drops landed changes and restacks. | `asc sync -n` |
| `undo`, `redo` | Reverses or repeats the last operation. `--op <id>` refuses unless that is the latest. | `asc undo -n` |
| `op log` | Lists operations, newest first. | `asc op log -n 10` |
| `op show` | Shows one operation. | `asc op show 12` |
| `op restore` | Puts the refs back to the state after an operation. | `asc op restore 12 -n` |
| `evolog` | The versions of a change. | `asc evolog -p` |
| `interdiff` | The diff between two versions of a change. | `asc interdiff abcd 1 2` |
| `change-id` | Prints the change id of a revision. `--short` prints 12 letters. | `asc change-id --short HEAD` |
| `share` | Saves a snapshot of the stack and prints its id. | `asc share -m "before the refactor"` |
| `continue`, `abort` | Continues or abandons an interrupted operation. | `asc continue -a` |
| `landed` | Whether a change has landed on the server's default branch. | `asc landed HEAD --json` |
| `stack info` | One change and its relatives. | `asc stack info HEAD` |
| `stack parent`, `children` | The parent or children of a change. | `asc stack children HEAD` |
| `stack trunk` | Shows, adds (`--add`) or removes (`--remove`) a trunk. | `asc stack trunk --add release/1.2` |
| `stack get` | Fetches a review, change, branch or snapshot into the repository. `-d` downstack only, `-u` upstack only. | `asc stack get '#12' --worktree ../review-12` |
| `stack freeze`, `unfreeze` | Marks a change as not to be rewritten, or lifts the mark. | `asc stack freeze HEAD` |
| `stack rename` | Renames the stack's branch. | `asc stack rename new-name` |
| `stack delete` | Deletes the stack's branch. `--upstack`, `--downstack`, `-c` closes the pull requests. | `asc stack delete --downstack -n` |
| `stack pop` | Pops the top change off the stack. | `asc stack pop -n` |
| `stack merge` | Asks the server to merge up to a change; `--now` lands at once. | `asc stack merge --now` |
| `stack exec` | Runs a command on each change of the stack, bottom to top, in one temporary worktree. A change whose tree did not move is not run again. | `asc stack exec -- cargo check` |
| `cloud status` | The upload queue and the last sync. | `asc cloud status` |
| `cloud sync` | Uploads the queue now. | `asc cloud sync` |
| `cloud join` | Uses another cloud workspace of this repository. | `asc cloud join laptop` |
| `cloud list` | The heads of a cloud workspace. `--all` includes old ones, `--user` another user's. | `asc cloud list --all` |
| `cloud release` | Uploads held drafts anyway. | `asc cloud release abcd` |
| `cloud purge` | Deletes drafts on the server. It only reports without `--yes`. | `asc cloud purge abcd --yes` |
| `live pause`, `resume` | Stops or resumes live updates for this worktree. | `asc live pause` |
| `live status` | Shows the live state and any conflict notice. | `asc live status` |
| `watch` | Streams workspace events. `--json` prints JSON lines; `--since <token>` resumes. | `asc watch --json` |
| `ws-api` | The workspace service as a command: `worktrees`, `conflicts`, `graph`, `read`. | `asc ws-api graph --preset 1 --limit 50` |

## 7. Code intelligence

All take `--json` and `--remote <name\|url>`. The scope is `--rev <rev>`, `--change <change>` or
`--stack [--diff-only]`; `--lang <lang>` narrows by language; a pathspec after `--` narrows by path.

| Command | What it does | Example |
|---|---|---|
| `search` | Index-backed text search. `-F` literal, `-E` regular expression, `-w` whole word, `-i` ignore case, `-C <n>` context. | `asc search -w retry_policy -C 2 -- src` |
| `sym def`, `refs`, `callers` | Definitions, references and callers of a symbol. `--kind <kind>` and `--at <path>:<line>` narrow. | `asc sym def retry_policy --kind function` |
| `sym outline` | The symbols of a file. | `asc sym outline src/client.rs` |
| `sym find` | Symbols whose name starts with a prefix. | `asc sym find retry` |
| `semantic` | Search by meaning. `-k <n>` results, `--vector` skips the text stage, `--rerank` reorders. | `asc semantic "where do we retry failed payments" -k 5` |
| `codeintel status` | Whether the index covers a revision. | `asc codeintel status --rev HEAD` |
| `codeintel worker` | Runs the local indexer. `--once` indexes HEAD and exits, `--now` does not wait for idle. | `asc codeintel worker --once` |
| `codeintel enable-embed` | Installs the embedding helper from `asc.releasesUrl` or `--from <dir>`. | `asc codeintel enable-embed --from ./release` |
| `codeintel upload-scip` | Uploads a SCIP index to the server. | `asc codeintel upload-scip index.scip --rev HEAD` |

## 8. Projects and CI

| Command | What it does | Example |
|---|---|---|
| `affected` | The projects a change touches. `--base`, `--head`, `--uncommitted`, `--format`, `--json`. | `asc affected --base master --json` |
| `projects` | Lists projects of a revision, or the owners of paths (`--owner <path>`). | `asc projects --format json` |
| `graph` | The project graph as JSON (`--format json`) or Graphviz (`--format dot`). | `asc graph --format dot` |
| `ci checkout` | The checkout step of a CI job from the host cache. The token comes from `ASC_TOKEN`. | `asc ci checkout --repo acme/payments --rev "$HEAD" --base "$BASE" --dir .` |
| `ci changed` | Prints `key=value` switches for `$GITHUB_OUTPUT` from the changed files. | `asc ci changed --repo acme/payments --base "$BASE" --head "$HEAD" --rules .asc/ci.toml` |
| `ci affected` | The same with the affected projects. | `asc ci affected --repo acme/payments --base "$BASE" --head "$HEAD"` |
| `ci warm` | Keeps the host cache warm; run by a timer. | `asc ci warm --repo acme/payments --ref master --host-cache /var/cache/asc` |
| `ci cache save` | Saves paths under a key. | `asc ci cache save deps-v1 target` |
| `ci cache restore` | Restores the newest match of a key. `--restore-keys` lists fallback prefixes. | `asc ci cache restore deps-v1` |
| `ci artifact upload` | Uploads files as a named artifact. `--root` sets the base directory, `--overwrite` replaces an artifact of that name. | `asc ci artifact upload build-logs target/logs --overwrite` |
| `ci artifact download` | Downloads an artifact into `--path` (default: the current directory). | `asc ci artifact download build-logs --path ./logs` |
| `ci artifact list` | Lists artifact names, newest first, optionally those starting with a prefix. | `asc ci artifact list build-` |

## 9. Agents and workflows

`agent`, `workflow` and `release` talk to a server; give `--remote` when the current repository has none.

| Command | What it does | Example |
|---|---|---|
| `agent run` | Starts an agent run from a task. `-w` follows it; `--max-cost`, `--max-seconds`, `--max-changes`, `--path` limit it. | `asc agent run --title "Add retry policy" --max-cost 2.50 -w "Add a retry policy to the HTTP client"` |
| `agent list` | Lists runs, newest first. | `asc agent list --state running` |
| `agent show` | Shows one run. | `asc agent show 7` |
| `agent watch` | Follows the events of a run. | `asc agent watch 7` |
| `agent cancel` | Stops a run. | `asc agent cancel 7 --reason "wrong task"` |
| `agent say` | Sends a message to a run. | `asc agent say 7 use exponential backoff` |
| `agent token create` | Makes a token for an agent that runs outside the server; the secret is printed once. | `asc agent token create --name ci-bot --agent builder --ttl 2h` |
| `agent token list`, `show`, `revoke`, `budget` | Lists, shows, stops or limits tokens. | `asc agent token revoke tok_1 --reason rotated` |
| `agent key add` | Stores an AI provider key; the key comes from a file, a variable or standard input. | `asc agent key add --provider anthropic --label team --key-env ANTHROPIC_API_KEY` |
| `agent key list`, `show`, `test`, `update`, `rm`, `models`, `usage` | Manages and checks keys. | `asc agent key test key_1` |
| `workflow list` | Lists the workflows on trunk. | `asc workflow list` |
| `workflow show` | Shows one workflow and its file. | `asc workflow show build` |
| `workflow validate` | Checks a workflow file. | `asc workflow validate build -F .asc/workflows/build.toml` |
| `workflow enable`, `disable` | Lets a workflow start from its triggers, or stops it. | `asc workflow enable build` |
| `workflow run` | Starts a run by hand. | `asc workflow run build --input target=release -w` |
| `workflow runs` | Lists runs. | `asc workflow runs build --state failed` |
| `workflow status` | Shows one run with its steps. | `asc workflow status 21` |
| `workflow watch` | Follows a run. | `asc workflow watch 21` |
| `workflow cancel` | Stops a run. | `asc workflow cancel 21` |
| `workflow approve`, `reject` | Decides an approval step. | `asc workflow approve 21 deploy --comment ok` |
| `workflow secret list`, `set`, `rm` | Manages the secrets steps read; `set` takes the value from `--key-file`, `--key-env` or standard input. | `asc workflow secret set DEPLOY_KEY --key-env DEPLOY_KEY` |
| `release list` | Lists releases, newest first (`--channel stable|pre-release|draft`). | `asc release list --channel pre-release` |
| `release show` | Shows a release with its provenance and the SHA-256 of each asset. | `asc release show v2.18.0` |
| `release next` | Counts the changes since the latest release and proposes the next tag. | `asc release next` |
| `release notes` | Writes notes (Added, Changed, Fixed) from the changes landed since a release. | `asc release notes --from v2.18.0` |
| `release create` | Records a release. `--asset FILE[=https://address]` records size and SHA-256 of a file; `--generate-notes`, `--pre-release`, `--draft`, `--build-source S --build-run ID` name the build whose checks become the provenance. | `asc release create v2.19.0 --generate-notes --asset dist/asc-linux.tar.gz=https://dl.example.org/asc-linux.tar.gz` |
| `release publish` | Publishes a draft. | `asc release publish v2.19.0` |
| `release download` | Downloads assets and refuses a file whose size or SHA-256 differs from the record. | `asc release download v2.18.0 --asset asc-linux.tar.gz -o dist` |

## 10. Server and API

| Command | What it does | Example |
|---|---|---|
| `serve` | Runs the server. `--config <asc-serve.toml>`, `--role <role>`, `--upstream <url>` (cache role). | `asc serve --config /etc/asc/asc-serve.toml` |
| `serve migrate` | Applies the database schema, then exits. | `asc serve migrate --config /etc/asc/asc-serve.toml` |
| `serve token create` | Mints a token and prints it once. `--repo`, `--user`, `--deploy`, `--ci`, `--read`, `--write`, `--admin`, `--expires`, `--label`. | `asc serve token create --user ada --repo 'acme/*' --write --expires 30d` |
| `serve token list`, `revoke` | Lists tokens (never their secrets), or revokes one by id. | `asc serve token list` |
| `serve ssh-key add` | Registers an SSH public key for a user or a deploy key. | `asc serve ssh-key add --user ada --key-file ada.pub` |
| `serve ssh-key list`, `revoke` | Lists keys, or revokes one by fingerprint or id. | `asc serve ssh-key revoke SHA256:abc` |
| `serve ssh host-key show` | Prints each host key's fingerprint and a `known_hosts` line. | `asc serve ssh host-key show` |
| `serve ssh host-key rotate` | Writes a new host key. Clients see a changed-key warning. | `asc serve ssh host-key rotate` |
| `serve admin import` | Imports git history into a mirror-mode repository. `--init` creates it. | `asc serve admin import --repo acme/payments --init --source https://git.example.org/acme/payments.git` |
| `serve admin verify-import`, `verify` | Checks an import against git, or the refs' closure and git map. | `asc serve admin verify --all` |
| `serve admin outbox drain` | Reports or waits for undelivered outbox rows. | `asc serve admin outbox drain --wait --timeout 300` |
| `serve admin set-mode` | Sets a repository's mode (`mirror` or `primary`) and `git_reads`. | `asc serve admin set-mode --repo acme/payments --mode primary` |
| `api` | Calls any RPC. `--list` lists them, `--docs` prints the reference, `--stdio` is the bridge the editor plugins use. | `asc api Repo/ListRefs --json '{"repo":{"name":"acme/payments"}}'` |
| `mcp` | The MCP server on standard input and output. Read-only unless `--write[=local\|remote]`; `--remote <url>` proxies to a hosted server. See [`mcp.md`](mcp.md). | `asc mcp --toolsets core,search` |
| `mcp install` | Writes the client configuration for a project or the user. | `asc mcp install --scope project` |

## 11. Plumbing

These match git's plumbing; scripts that use git's can use these.

| Command | Example |
|---|---|
| `show-ref` | `asc show-ref --heads` |
| `for-each-ref` | `asc for-each-ref --format='%(refname)' refs/heads` |
| `symbolic-ref` | `asc symbolic-ref HEAD` |
| `update-ref` | `asc update-ref refs/heads/topic HEAD` |
| `check-ref-format` | `asc check-ref-format --branch topic` |
| `var` | `asc var GIT_AUTHOR_IDENT` |
| `ls-files` | `asc ls-files --cached` |
| `ls-tree` | `asc ls-tree -r HEAD` |
| `cat-file` | `asc cat-file -p HEAD` |
| `hash-object` | `printf 'x' \| asc hash-object --stdin` |
| `update-index` | `asc update-index --refresh` |
| `write-tree` | `asc write-tree` |
| `read-tree` | `asc read-tree HEAD` |
| `diff-tree` | `asc diff-tree -r HEAD~1 HEAD` |
| `diff-index` | `asc diff-index --cached HEAD` |
| `diff-files` | `asc diff-files` |
| `commit-tree` | `asc commit-tree -m "msg" "$(asc write-tree)"` |
| `mktree` | `asc ls-tree HEAD \| asc mktree` |
| `maintenance`, `prune`, `repack` | `asc maintenance run` (accepted; there are no packfiles to maintain, so they do nothing) |
