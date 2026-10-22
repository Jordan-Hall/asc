# Stacked changes, stacked PRs and review features in asc core

Status: design for SPEC §19 item 7 (user decision 2026-10-22, last section of `scion-checklist.md`). This file is not normative until its content is folded into SPEC.md and PLAN.md. §7 lists the edits those files need. A design review (2026-10-22) was applied to this file; its rejected points are under "Review notes" at the end.

Inputs: the Graphite docs, CLI changelog and blog; GitLens and GitKraken help pages; the docs of jj, Sapling, Gerrit, git-branchless, ghstack, spr, Git Town, Mercurial evolve and GitHub stacked PRs (research of 2026-10-22). The fixed rules come from SPEC §0, §2, §7, §8 and §11, and from `CARRY.md`.

Fixed rules this design obeys:
- git commands keep git's exact semantics, output and exit codes;
- stack features are additive verbs;
- there is no IDE extension, so GitLens/GitKraken-style features are data APIs;
- the default branch is `master`;
- there are no whole-repo scans;
- there is no LFS.

---

## 0. Decisions

| # | Decision | Why |
|---|---|---|
| D1 | **The unit of review is the change (one commit).** A stack is the chain of draft changes from a trunk to a head. Branches are optional labels on changes. | jj, Sapling, Gerrit, spr and ghstack all use this model. Graphite's "one commit per branch" convention is the same model with a branch name on every commit. |
| D2 | **Change identity is a `ChangeMeta` sidecar object.** It is content-addressed (`m/<meta id>`, BLAKE3-verified like every other object) and names the changeset it describes. It is not part of the hashed changeset and not part of the git commit. | Any identity that is hashed or rendered into the commit changes the git SHA of every commit asc makes. That fails the compat gate ("ids are not normalised", SPEC §17.1) and breaks export determinism (§2.1). Content addressing keeps SPEC §0 decision 5 (every read is verified) true for the sidecar too. |
| D3 | **Stack parents come from the commit graph plus change ids.** No per-branch parent metadata is stored. | There is nothing to drift or repair, so `gt track` is unnecessary. Obsolete and landed parents are found through the change index and `land_map`. |
| D4 | **No existing git command changes behaviour, under either `git` or `asc`, with or without configuration.** Stack features are new verbs only. Automatic restack after a git rewrite is not offered (the `stack.restackOnRewrite` opt-in was removed, see Review notes). | asc's git commands are the git commands, and the checklist rule ("never change what an existing git command does") has no opt-in exception. Two behaviours for one command name would confuse humans and agents. |
| D5 | **When argv[0] is `git`, a stack verb runs only where git would print "not a git command".** A user alias or a `git-<verb>` executable on PATH wins. The verb match is exact and happens before `help.autocorrect`: autocorrect never runs for a name that is a stack verb, and stack verbs are never autocorrect candidates, so a typo of a git command is corrected as in git. | git-extras ships `git undo`, `git sync` and `git squash`, and gitext ships `git stack`. Existing setups must keep working (§11.4), and git's typo handling stays byte-identical. |
| D6 | **The local operation log groups the existing `ref_log` rows by `op_id`.** It adds redacted argv, the workspace and an optional working-copy snapshot. It is bounded to 1,000 ops or 90 days. `asc undo` covers every ref-moving command, git ones included, and restores the tracked working copy for every command that destroys it (§2.3). Untracked and ignored files that `clean` deletes are a documented gap. | This gives jj/Sapling-style undo for about 200 B per command. It is recorded invisibly. |
| D7 | **Restack runs in memory.** It uses a metadata rebase when paths are disjoint (§4.4). Only a conflicted change is materialised, in git's own `rebase-merge/` state. | It is instant, does not disturb the working tree, and leaves `git status` and `git rebase --continue` behaving as git. |
| D8 | **A stacked PR is stored with the trunk as `base_branch`, and its diff base (`merge_base`) is its own parent commit.** The PR is found by change id through the AGit topic `asc-<change>`. Users see the checklist behaviour: the base shown is the parent PR, and it becomes the trunk when the parent merges, with no retarget step. The checklist records the user-visible behaviour; the storage is an implementation choice (resolved 2026-10-22). | There is no branch per change, no synthetic base branch (ghstack/spr) and no retarget dance. A metadata rebase keeps the PR's diff unchanged. The commits tab and diff show only the change because both key off `merge_base` (§4.1). |
| D9 | **The asc server owns forge-neutral change facts** in a new `Changes` service: the index, versions, stack shape, mergeability, overlaps and interdiff. **Forgejo owns social state**: reviews, inbox and queue. Forgejo pushes a summary back to asc (`SetReviewState`). | Scion's API-first rule, and `repo.proto` stays forge-neutral (§7). The CLI and plugins never call a forge API for stack data. |
| D10 | **A stack lands as one `UpdateRefs(LAND)` call with the prefix's top change as the new tip.** The land is atomic, is a pushrebase and takes no global lock. The "merge queue" is Forgejo's ordered merge-when-ready built on this primitive. | Post-merge testing stays as in §12.6. There is no speculative CI and no batching. Revalidation is optional and only runs when the land needed a content merge. |
| D11 | **After a parent lands, the server restacks open children (`RestackChanges`).** An approval survives when the change's own `changes` list is unchanged. | This is Graphite's server-side restack. The "patch unchanged" check is O(1) on Bonsai changesets. |
| D12 | **GitLens/GitKraken features are delivered as data:** Workspace IPC plus `asc.v1` RPCs (`CommitGraph` with lanes, enriched `Blame`, `LineHistory`, `Compare`, `Worktrees`, `Stacks`, `Conflicts`). ASC's plugins and the Forgejo fork render them. Everything is free, and AI features stay out of core. | The most-loved features (blame, history, graph) are served from derived data. The paywall was the top community complaint against GitLens. |

---

## 1. Feature matrix

Classes: **CC** core client (asc CLI, engine, `state.db`); **CS** core server derived data or RPC (`asc serve`); **FJ** Forgejo fork (UI, API, policy); **API** plugin API only (asc provides data and ASC's plugins render it); **OUT** out of scope. Rows are deduplicated across products.

### 1.1 Identity and model

| Feature | Seen in | Class | Reason |
|---|---|---|---|
| Stable change id that survives amend and rebase | jj, Gerrit, spr, ghstack, Phabricator, Sapling | CC+CS | The content-addressed `ChangeMeta` sidecar (§2.1). The server indexes it to map pushes to reviews. |
| Rewrite (mutation/obsolescence) records | hg evolve, Sapling, git-branchless, jj evolog | CC+CS | `ChangeMeta.preds` travels with the objects. It drives restack, divergence and evolog. |
| Parent tracking / stack metadata | Graphite, Git Town | CC | Derived from the graph plus change ids (§2.2). Nothing is stored. |
| `track` / `untrack` / trunk init | Graphite | CC | `asc stack trunk`. Every draft change is tracked automatically. `asc.stack.exclude` (ref globs) replaces `untrack`. |
| Multiple trunks | Graphite | CC | `asc.stack.trunk` is multi-valued and defaults to the default branch (`master`). |
| Immutable/public protection (phases) | hg, Sapling, jj | CC | A change is mutable when it is not reachable from a trunk or an `asc.stack.immutable` ref. Tags are not checked (O(tags) per change); list release refs in `asc.stack.immutable`. |
| Divergence detection | jj, evolve | CC+CS | Locally: more than one successor head. On the server: `CHANGE_DIVERGED` on submit. |
| Orphan / needs-restack detection | git-branchless, Sapling, Gerrit "Not current" | CC+CS | The obsolete-parent rule (§2.2). `Stack.parent_current` reports it. |
| First-class conflicts stored in commits | jj | OUT | SPEC §0 already excludes it. Instead, restack pauses on the first conflicted change, and `sync` skips and reports conflicting stacks. |
| Working copy as a commit, with auto-snapshot | jj | OUT | It contradicts git's staging model. Instead, the op log snapshots the working copy before verbs that touch it. |
| Revset language | jj, Sapling, git-branchless | CC (subset) | Only under `-r/--revset`, which accepts `stack()`, `draft()`, `trunk()`, `mine()`, `mutable()` and `git(<spec>)` with `\|`, `&` and `~` (difference). Positional arguments are never revsets, so git's `HEAD~2` keeps its meaning. Git revspecs are unchanged. |
| Operation log, undo/redo, op restore | jj, Sapling, git-branchless, Graphite, GitKraken | CC | §2.3. |
| Reword, uncommit, abandon | jj `describe`/`abandon`, Sapling `metaedit`/`uncommit`/`unamend`, Graphite | CC | `asc reword`, `asc uncommit`, `asc unamend`, `asc abandon` (§3). |
| Topic spanning several repos | Gerrit | OUT | Stacks are single-repo. ASC's second repo is composed read-only (§10.6). |

### 1.2 Authoring and editing stacks

| Feature | Seen in | Class | Reason |
|---|---|---|---|
| Create a stacked change (`--insert`, `--onto`) | Graphite, Sapling, jj | CC | `asc create`. |
| Amend with automatic descendant restack (`--into`) | Graphite, Sapling, jj | CC | `asc modify`. |
| Absorb hunks into the right downstack change | Graphite, Sapling, jj, git-absorb | CC | `asc absorb` uses `blame.v1` line origins restricted to mutable changes. |
| Restack | Graphite, Sapling, git-branchless | CC | `asc restack`, in memory (D7). |
| Split (by commit, hunk or file), squash, fold | Graphite, Sapling, jj | CC | `asc split`, `asc squash`, `asc fold`. |
| Move a subtree, insert, reorder | Graphite, git-branchless, jj, Sapling histedit | CC | `asc move` and `asc reorder`. `git rebase -i` stays git's. |
| Rename, delete, pop with metadata consistency | Graphite | CC | `asc stack rename\|delete\|pop`. |
| In-memory rebase | git-branchless, jj | CC+CS | `rebase_metadata` (§4.4) locally, and `Rebase`/`RestackChanges` on the server. |
| Continue / abort a paused multi-change op | Graphite, Sapling | CC | `asc continue` / `asc abort` on git's `rebase-merge/` plus `asc-stack-op.json`. |
| Worktree-aware stacking | Graphite | CC | Never rewrite a branch that another workspace has checked out (§9.3 rule). Skip it and report it. |
| Run fixers/tests on every change | jj fix, git-branchless test | CC (v1.x) | `asc stack exec`, run in ephemeral worktrees with results cached by `TreeId`. |
| AI branch names, messages, Composer/Recompose | Graphite, GitLens, GitKraken | OUT | AI belongs in ASC's agents and plugins. Core exposes diffs and accepts plans through the verbs. |

### 1.3 Sync, submit, collaborate

| Feature | Seen in | Class | Reason |
|---|---|---|---|
| Sync: fetch trunk, drop landed changes, restack, delete merged | Graphite, git-branchless, Git Town, gh stack | CC | `asc sync`. Landed changes are found by change id and `FindLanding`, never by matching messages. |
| Stack-aware push plus PR create/update | Graphite, spr, ghstack, sl pr, jj gerrit | CC+CS+FJ | `asc submit` sends one `UpdateRefs` with AGit refs per change (§4.2). |
| Synthetic base branches | ghstack, spr | OUT | Not needed: the PR diff base is the parent commit (D8). |
| Auto-named push refs per change | jj, gh stack | CS | No refs at all: AGit topic `asc-<change>`, with `refs/pull/<n>/head` as the only server ref. |
| Old versions kept reachable | Gerrit, ghstack, Sapling archive branch | CS | `change_versions` rows are GC roots, not refs (§4.4). |
| Fetch a colleague's stack; frozen branches | Graphite `gt get`, `sl pr pull` | CC | `asc stack get` with frozen-by-default. `--worktree` opens it in a new worktree. |
| Start review in a new worktree | GitLens, GitKraken | CC | `asc stack get <pr> --worktree <path>`. A plain worktree (U54w) is under 1 s; a virtual one follows once U57/U58 land. |
| Share uncommitted work by link (Cloud Patches) | GitLens, GitKraken | CC | `asc share`: snapshot, then `Ingest`, then the link (§8.7). The objects are rooted by `refs/snapshots/<id>` with an expiry (§8.8). The ACL is the repo's read permission. |
| Start work from an issue | GitLens, GitKraken | API | ASC's plugin. `asc create --branch` takes the name. |
| Submit to GitHub/GitLab | Graphite, GitLens | OUT | ASC's forge is Forgejo. `asc submit` needs an asc server. |

### 1.4 Review and landing

| Feature | Seen in | Class | Reason |
|---|---|---|---|
| Stack map / related changes on each PR | Graphite comment, Gerrit, GitHub stack map, ReviewStack | FJ from CS | A native panel built from `Changes.Stack`. It replaces Graphite's stack comment (a workaround for GitHub), which is OUT. |
| Per-change diff and review | ReviewStack, Gerrit, Phabricator | FJ | Each PR is one change. Its diff base is its parent commit (D8). |
| Versions and interdiff; "since my last review" | Graphite, Gerrit, jj evolog | CS+FJ | `change_versions` plus `InterDiff`. The reviewed version comes from Forgejo's review `commit_id`. |
| Mergeability block mid-stack | Graphite, GitHub | FJ | The merge button is replaced by "Merge stack (N)". There is no separate status check. |
| Merge the bottom N / ancestors with a change | Graphite, Gerrit, GitHub | FJ+CS | One `UpdateRefs(LAND)` for the prefix (D10), requested through `Changes.RequestMerge` (§4.3). |
| Auto-retarget and rebase upstack after a merge | Graphite, GitHub | CS+FJ | `RestackChanges`. No retarget is needed because the base is already trunk. |
| Server rebase button | Gerrit, GitHub | FJ | Calls `Rebase` (§8.11) or `RestackChanges`. |
| Approvals survive a restack when the patch is unchanged | GitHub | FJ+CS | `RestackChangesResponse.patch_unchanged`. |
| Merge when ready, cascading down | Graphite | FJ | Extends Forgejo's `pull_auto_merge` to the stack prefix. |
| Stack-aware queue, atomic per stack | Graphite, GitHub | FJ over CS | Ordered per target branch, with one in-flight land per branch. Pause is supported. |
| Speculative CI, batching, bisection, fast-track | Graphite MQ | OUT | Scion post-merge testing (§12.6). Land is a pushrebase on disjoint paths. |
| External merge-queue integration | Graphite | OUT | No external queue. |
| CI optimizer for upstack PRs | Graphite | FJ | Replaced by per-change affected: `asc affected --base <parent>` from the Actions context. |
| Stack metadata in CI events | GitHub `pull_request.stack` | FJ | Actions context `asc_stack_*` (K14 extension). |
| Conflict prediction against trunk | GitLens, GitKraken | CS | `Mergeability`, one overwritten row per head with a path-disjoint fast path. |
| Teammate overlap detection | GitKraken Conflict Prevention | CS+FJ | `Overlaps` over the `open_paths` index. |
| Code Suggest (edits beyond the diff) | GitLens, GitKraken | FJ (v1.x) | A suggested changeset attached to a review, applied as a new version. |
| PR inbox / Launchpad, with whose-turn, pin and snooze | Graphite, GitLens, GitKraken | FJ | Forgejo owns the review data. ASC's plugins call its API. |
| Automations, Insights, Slack review | Graphite, GitKraken | OUT | Forgejo webhooks and Actions cover the needed cases. |
| AI review, chat, agents on the PR, MCP server | Graphite, GitLens, GitKraken | OUT | Agents use the `--json` verbs and the `Rebase` resolutions API. |

### 1.5 History and code insight (GitLens/GitKraken)

| Feature | Class | Reason |
|---|---|---|
| Inline, status-bar and hover blame | API | `Workspace.Blame` gains an unsaved-buffer input plus change and review per origin (§5). |
| File blame gutter, heatmap, CodeLens authors | API | Rendered from one blame response. The plugin's language server supplies the symbol ranges. |
| File-changes annotations | API | `Workspace.Diff` (exists). |
| Revision navigation, file history following renames | API | `Log(paths, follow)` plus `ReadFile`. Served from the path index or `pathlog`. |
| Line history (`log -L`) | CS+API | The new `LineHistory` RPC, with a scan budget (§5). |
| Commit details and commit-to-PR association | CS+API | `ReadChangeset` plus `ResolveChanges.review`. |
| Commit graph (lanes, ref pills, WIP rows, PR badges) | CS+API+FJ | The `CommitGraph` RPC with server-computed lanes, shared by plugins and the Forgejo page. It is separate from `asc graph` (the project graph, SPEC §12A.9). |
| Graph search operators (message, author, file, pickaxe, dates) | CS | `LogRequest` fields, plus `pickaxe` (expensive class, requires a path, bounded by a scan budget). Natural-language search is OUT. |
| Minimap, filters, hide/solo/pin, "smart" branch view | API | Computed from `CommitGraph` pages. The smart view comes from `Stacks` (current, upstream, trunk). |
| Graph actions (squash, drop, reword, drag-and-drop rebase or merge) | API | The plugin calls the CLI verbs with `--json` (`asc reword`, `asc abandon`, `asc reorder`). |
| Interactive rebase editor and multi-commit cherry-pick editor | API | `GIT_SEQUENCE_EDITOR` (git's own), `asc reorder --json`, and `--dry-run --json` for predicted conflicts. |
| Three-way merge editor | API | `Workspace.Conflicts` (base, ours, theirs, marked). |
| Visual file history (numstat per author over time) | CS+API | The `diffstat.v1` derived type and `Log.with_stats`. |
| Search & Compare (ahead/behind, files, two or three dots) | CS+API | The `Compare` RPC. |
| Branches view with upstream and PR state; stashes view | API | `for-each-ref` atoms (exist), plus `Stacks` and `asc stash list` (exist). |
| Worktrees view and multi-WIP rows | API | `Workspace.Worktrees`, built from the journal. |
| Autolinks | CS | `[autolinks]` in `.asc/review.toml`, read at the revision and returned by `RepoConfig`. |
| Open on remote, deep links, avatars | API | `/.well-known/asc` gains a `web` URL template. Forgejo serves avatars. |
| Signature badges | existing | The `%G?` placeholders (§11.1) and Forgejo verification. |
| Command palette, terminal links, repo tabs, profiles, Gitflow, multi-repo workspaces, work items, agent sessions | OUT | These are IDE or desktop UI, or products GitKraken itself deprecated. |

---

## 2. Data model

### 2.1 Stable change identity

| Option | Hashing | amend / rebase | import / export | git SHA parity | Verdict |
|---|---|---|---|---|---|
| jj-style field in `Changeset` | Changeset envelope v2, inside `ChangesetId` | carried by asc rewrites | Must render as a `change-id` git header, or a re-import changes the id (§0 decision 2) | Every asc commit's SHA differs from git's, so the compat gate fails | rejected |
| Gerrit trailer `Change-Id:` in the message | inside the message, so hashed | survives both, but users and tools edit messages and a commit-msg hook is needed | renders in the message, deterministic | Differs from git, because git adds no trailer; the message text visibly changes | rejected (read on import only) |
| Extra-map key `asc.change` | inside `ChangesetId` | carried (§8.5 5a keeps `asc.*` extras) | Must render as a header, as above | fails as the field does | rejected |
| **Content-addressed `ChangeMeta` sidecar that names its changeset** | **not hashed** into `ChangesetId` or the git SHA | each asc rewrite writes one for the new changeset, with the same change and `preds=[old]` | Nothing renders, so mirror and bridge bytes are unchanged. Import derives one deterministically when the git commit carries a jj `change-id` header or a Gerrit trailer. | equal to git | **chosen** |

```rust
pub struct ChangeId([u8; 16]);                 // random (OsRng) at creation
pub struct MetaId([u8; 32]);                   // BLAKE3 of the ChangeMeta envelope
pub struct ChangeMeta {
    pub version: u8,                           // 1
    pub cs: ChangesetId,                       // the changeset this describes (binds the object to one commit)
    pub change: ChangeId,
    pub preds: Vec<ChangesetId>,               // sorted, deduplicated; empty for a new change
    pub op: RewriteOp,
}
pub enum RewriteOp { New = 0, Amend = 1, Rebase = 2, Absorb = 3, Split = 4, Fold = 5, Squash = 6, Land = 7, Restack = 8, Import = 9 }
```

**Storage.**
- `ChangeMeta` uses envelope kind 6 under key `m/<meta id>`, with `ObjKind::CHANGE_META = 9`. It is content-addressed like every other object: `put` and every read verify the BLAKE3 of the envelope (SPEC §0 decision 5), so caches cannot serve a corrupt one.
- The map `cs → meta id` is index data, not object data: `changes.meta` locally and on the server (§2.4, §4.4), and `LogEntry.meta` on the wire. The first meta indexed for a changeset wins. A second, different meta for the same changeset (only possible for byte-identical commits made independently) is stored but not indexed.
- Every tier caches `m/` objects as immutable. A meta is collected with its changeset.
- **Validation on ingest.** The server accepts a meta only if `meta.cs` is a changeset in the same push closure (or already stored), so a meta cannot be attached to someone else's commit. `preds` may name any stored changeset.

**Rules.**

| Operation | Change id of the result | `preds` |
|---|---|---|
| `commit`, `asc create` | new | [] |
| `commit --amend`, `asc modify`, `asc absorb` (each target) | same | [old] |
| `rebase`, `rebase -i` pick/reword/edit, `pull --rebase`, restack, server `LAND`/`Rebase`/`Merge(REBASE)`/`RestackChanges` | same | [old] |
| `rebase -i` squash/fixup, `asc fold`, `asc squash` | the earliest folded change's | [all folded] |
| `asc split` | The piece that keeps the original message keeps the id; the other pieces get new ids. | [old] on every piece |
| `cherry-pick`, `revert`, merge commits, `stash` | new (a copy is a new change); stash snapshots get none | [] |
| `Merge(SQUASH)` of a PR submitted with asc | the PR's change | the sources |
| git import | jj `change-id` header, decoded; else Gerrit `Change-Id` trailer → `blake3_derive("asc v1 change gerrit", scope \|\| 0x00 \|\| value)[..16]`, where `scope` is the first import ref (in sorted ref order) that reached the commit, recorded in the written meta so later imports keep it; else none. A commit reachable from an immutable ref is never obsolete or divergent (§2.2). | [] |

**Display.**
- The full form is 32 reverse-hex letters `k`–`z` (jj's alphabet: hex digit d becomes `'z' - d`). The short form is 12 letters.
- These can never be confused with the hex ids of §2.1.
- New verbs resolve a positional argument in this order: exact branch name, git revspec (`~` and `^` keep git's meaning), unique change prefix (at least 4 letters). A change id is read with `asc change-id <rev>` (prints the 32-letter id; `--short` prints 12) or `asc stack info --json`. Git commands gain no flags: real git echoes unknown `--options` back from `rev-parse`, so `rev-parse --change-id` is not added.

**Transport.**
- Push: the push closure (§6.3) includes the `m/` objects of the new changesets.
- Fetch: `LogEntry.change` (new field 8) and `LogEntry.meta` (new field 10) carry the ids, so fetch makes no extra round trip.
- Local index: `state.db.changes` (§2.4). Server index: `changes` (§4.4).
- Bucket remotes store `m/` like any other object.
- Rolling upgrade: the server advertises `changes.v1` in `Version.features`, and clients upload `m/` objects only after seeing it. Old readers never meet the new key kind.

### 2.2 Stack derivation (no stored parent metadata)

| Term | Definition | Cost |
|---|---|---|
| trunk tips | For each `asc.stack.trunk` value (default: the repo default branch), `refs/remotes/<remote>/<t>` if present, else the local branch | O(trunks) |
| immutable | Ancestors of any trunk tip or any ref matching `asc.stack.immutable` (default `refs/remotes/*/release/*`). Tags are not checked. | `is_ancestor` through generations and skip lists (§4.3), O(trunks + patterns) per change |
| draft change | A non-immutable changeset that has a `ChangeMeta` | O(1) lookup |
| heads | Local branches, plus every workspace `HEAD`, plus (with `--remote`) changes in my open reviews | O(local refs) |
| stack of a head | First-parent walk from the head while the changeset is a draft. A merge changeset ends the walk; stack verbs never rewrite merges. | O(stack length × log n) |
| stack trunk | The trunk whose tip has the highest-generation merge base with the head | O(trunks × log n) |
| parent change | The change of `parents[0]` when that is a draft; otherwise the change is the stack bottom | O(1) |
| branch parent (Graphite view) | The nearest ancestor change that carries a branch label | O(stack) |
| obsolete | Changeset X is obsolete when a visible changeset with the same change has X in its `preds` closure. "Visible" means reachable from a head or from the latest op-log view. A changeset reachable from an immutable ref is never obsolete. | local index |
| needs restack | The parent is obsolete, or the parent's change has landed (`landed_as` from the change index or `land_map`) | O(stack) |
| behind | The stack base is not the trunk tip (reported, not an error) | `is_ancestor` |
| divergent | One change has more than one visible successor head. Changesets reachable from an immutable ref are ignored, so Gerrit cherry-picks that share an id across branches are not divergent. | local index |
| frozen | A row in `stack_frozen`. Frozen changes are never rewritten locally. | O(1) |

### 2.3 Operation log (local, bounded)

- **One op per command.** Every ref-moving command already writes `ref_log` rows with an `op_id` (§3.4). The op log adds one row per `op_id`, in the same `state.db` transaction. This includes git commands. Recording does not change any output. `argv` is stored redacted: URL credentials (`scheme://user:pass@`) become `scheme://***@`, the value of every `-c key=value` becomes `***`, and the value of any `--password`, `--token` or `--auth*` option is dropped.
- **Working-copy snapshot.** Before a verb that rewrites the working copy (`modify`, `absorb`, `split`, `restack` when it pauses, `undo`) **and before every git command that can destroy tracked changes** (`reset --hard`, `checkout -f`/`switch -f`/`--discard-changes`, `checkout <paths>`, `restore` of the worktree, `stash drop` and `clear`, `rebase --abort`, `merge --abort`), the dirty tracked files and the staging rows are saved as an `asc.snapshot` changeset in `store/`, as `stash create` does. The cost is O(dirty files). **Documented gap:** untracked and ignored files removed by `clean` (or replaced by `checkout`/`switch`) are not captured, and `asc undo` says so in a `hint:` when the op's argv is one of these.
- **`asc undo`** applies the inverse of the op's `ref_log` deltas, with a CAS against each ref's recorded new value. It restores `HEAD` and, if present, the snapshot.
  - It refuses when a ref has moved since the op, or when it would move a branch that another workspace has checked out, unless `-f` is given.
  - Undo is itself an op, so `asc redo` undoes the undo.
  - Remote effects (push, submit, land) are never undone. Undo prints a `hint:` naming them.
- **Bounds.** Ops older than `asc.opLog.maxDays` (90) or beyond `asc.opLog.maxOps` (1,000) are pruned. `ref_log` rows follow git's `gc.reflogExpire`. A snapshot that only pruned ops reference is removed from `store/` by `asc gc`. The cost is about 200 B per op plus snapshots. `asc.opLog=false` disables recording.

```sql
op_log(repo_id, seq INTEGER PRIMARY KEY, op_id BLOB UNIQUE, ws_id TEXT, at INT, argv TEXT /*redacted*/, kind TEXT,
       head_before BLOB, head_after BLOB, wc_snapshot BLOB NULL, undoes_seq INT NULL)   -- ref deltas = ref_log rows with op_id
```

### 2.4 Other local state (`state.db`)

```sql
changes(repo_id, cs BLOB, change BLOB, meta BLOB /*MetaId*/, op INT, PRIMARY KEY(repo_id, cs)) + INDEX (repo_id, change)
change_preds(repo_id, cs BLOB, pred BLOB, PRIMARY KEY(repo_id, cs, pred)) + INDEX (repo_id, pred)
stack_frozen(repo_id, change BLOB PRIMARY KEY)
review_cache(repo_id, change BLOB PRIMARY KEY, review BLOB /*postcard ReviewState*/, fetched_at INT)   -- TTL 60 s
```

Config lives in git-format config under the `asc.stack.*` keys (SPEC §11 puts every asc key under `asc.*`, and `stack.*` belongs to other tools such as git-stack): `trunk` (multi-valued), `immutable`, `exclude`, `branchTemplate` (default `{user}/{slug}`) and `submit.draft`. A paused stack op keeps its remaining plan in `<gitdir>/asc-stack-op.json`, next to git's state files (§4.4).

---

## 3. Commands

Every verb is asc-only. Under argv[0] `git`, the D5 precedence rule applies. Every verb takes `--json` (schemas below) and `-n/--dry-run` where it mutates. Exit codes: 0 done or nothing to do; 1 paused on a conflict (as `git rebase` does); 2 ambiguous (a fork or several candidates, listed in `candidates` under `--json`); 128 fatal; 129 usage.

| Verb and key flags | Does | Graphite | Sapling | jj |
|---|---|---|---|---|
| `asc create [<branch>] [-m <msg>] [-a\|-u\|-p] [--insert] [--onto <rev>] [--no-branch]` | Creates a new change on `HEAD` (or on `--onto`) from the staged changes; the change is empty if nothing is staged. `--insert` reparents `HEAD`'s children onto it. Names a branch from `asc.stack.branchTemplate` unless `--no-branch`. Runs the commit hooks (see "Hooks and signing"). | `create` | `commit` | `new`/`commit` |
| `asc modify [-a\|-u\|-p] [-c] [-m <msg>\|-e] [--into <change>]` | Amends the current change (`-c` adds a new change instead, `--into` amends a downstack change without checking it out), then restacks the descendants. Runs the commit hooks. | `modify` | `amend [--to]` | `squash [--into]` |
| `asc absorb [-n] [-f] [-p] [--into <revset>]` | Moves each staged hunk (or each unstaged tracked hunk when nothing is staged) to the nearest mutable change it does not commute with, then restacks. Ambiguous hunks and new files stay where they are. Shows a preview unless `-f`. | `absorb` | `absorb` | `absorb` |
| `asc restack [<change>] [--downstack\|--upstack\|--only]` | Rebases each change whose parent is obsolete or landed, or whose base is behind its parent. Runs in memory and pauses on the first conflict. | `restack` | `restack` | automatic |
| `asc continue [-a]` / `asc abort` | Finishes a paused op (running `rebase --continue` first if needed) or restores the op's before-state | `continue`/`abort` | `continue`/`abort` | – |
| `asc sync [--no-fetch] [--no-restack] [-d] [--all-trunks]` | Fetches; fast-forwards local trunks that no other workspace has checked out; drops landed changes; offers to delete merged branches (`-d` deletes them); restacks each stack onto its trunk. Stacks with conflicts are skipped and reported. | `sync` | `pull --rebase` | `git fetch` + rebase |
| `asc submit [<change>] [-s] [--draft\|--publish] [--reviewer <u>…] [--update-only] [--auto-merge] [--force]` | Pushes bottom..change (or the whole stack with `-s`) as one `UpdateRefs` of `refs/for/<trunk>/asc-<change>` refs, creating or updating one PR per change (§4.2). Refuses if a change needs a restack, is divergent or is frozen. | `submit` | `pr submit` | `git push -c`, `gerrit upload` |
| `asc stack merge [<change>] [--now]` | Asks the server to merge bottom..change when ready, through `Changes.RequestMerge` (§4.3). `--now` lands at once if every PR in the prefix is mergeable. It is not `asc landed` (SPEC §8.9), which only answers whether a revision has landed and changes nothing. | `merge` | – | – |
| `asc sl [-s] [-a] [-r] [--remote] [--json]` (also `asc smartlog`) | Shows draft changes relative to trunk, with branch labels, `@`, needs-restack, divergent, frozen, worktree and (with `--remote`) PR number, state, checks and version | `log short` | `sl`/`ssl` | `log` |
| `asc up [n] [--to <b>]`, `asc down [n]`, `asc top`, `asc bottom` (aliases `next`, `prev`) | Switches by relationship. At a fork it prompts; with `--json` (or without a terminal) it exits 2, moves nothing and lists the candidates. | `up`/`down`/`top`/`bottom` | `next`/`prev`/`goto top` | `next`/`prev --edit` |
| `asc reword [<change>] [-m <msg>\|-e]` | Edits the message of any mutable change without touching its tree, then restacks the descendants. Runs `commit-msg`. | – | `metaedit` | `describe` |
| `asc uncommit [<change>] [-a\|-p\|<pathspec>…]`, `asc unamend` | `uncommit` moves the change's tree (or the chosen paths) back into the working copy and staging, leaving the change with the rest (empty if all). `unamend` restores the previous version of the current change from its `preds`. | – | `uncommit`/`unamend` | `restore --from` |
| `asc abandon [<change>…] [--keep-descendants]` | Drops mutable changes from their stack and restacks the descendants onto the parent. The commits stay reachable through the op log (`asc undo`). | `delete` | `hide` | `abandon` |
| `asc change-id [--short] [<rev>]` | Prints the change id of a revision (empty and exit 1 for a commit without one). | `info` | – | `log -T change_id` |
| `asc split [--by-commit\|--by-hunk\|--by-file <pathspec>…]` | Splits one change into a stack. `--by-file` moves the matching files into a new parent change. | `split` | `split` | `split` |
| `asc squash [-m <msg>\|-e\|-n]` | Collapses the changes of the current branch into one | `squash` | `fold` | `squash` |
| `asc fold [--keep] [--stack]` | Folds a change into its parent and reparents its children | `fold` | `fold --from` | `squash` |
| `asc move --onto <rev> [--source <change>] [--only]` | Reparents a change and its descendants (`--only` reattaches the descendants to the old parent) | `move` | `rebase -s -d` | `rebase -s/-r -o` |
| `asc reorder [--stack]` | Editor list of changes, bottom to top; rewrites the parents and restacks | `reorder` | `histedit` | `rebase -A/-B` |
| `asc stack rename <new>`, `asc stack delete [--upstack\|--downstack] [-c]`, `asc stack pop` | Branch lifecycle. Children are restacked onto the parent. `-c` also closes the PRs. | `rename`/`delete`/`pop` | `hide` | `abandon` |
| `asc stack get <pr\|change\|branch> [-d] [-u] [--unfrozen] [--worktree <path>]` | Fetches a stack from the server, frozen by default. `--worktree` opens it in a new plain worktree. | `get` | `pr pull` | `git fetch` |
| `asc stack freeze\|unfreeze [<change>]` | Toggles local protection | `freeze` | – | immutable |
| `asc stack trunk [--add\|--remove <b>]`, `asc stack info [<change>]`, `asc stack parent\|children` | Configuration and relationship reads | `trunk`/`info`/`parent`/`children` | – | – |
| `asc undo [-n] [-f]`, `asc redo`, `asc op log`, `asc op show <op>`, `asc op restore <op>` | §2.3 | `undo` | `undo`/`redo` | `undo`/`redo`/`op` |
| `asc evolog [<change>] [-p]`, `asc interdiff <change> [<v1> <v2>]` | The versions of a change (local and server) and the diff between versions | PR versions | `log --hidden` | `evolog`/`interdiff` |
| `asc share [-m <msg>]` | Snapshots the working changes, `Ingest`s them, writes `refs/snapshots/<id>` with an expiry (default 30 days, `UpdateRefs.ttl_seconds`, SPEC §8.8) so the objects have a GC root, and prints `asc:<id>` and a web link. Recipients run `asc stack get asc:<id>`. | – | – | – |
| `asc stack exec [--revset <r>] -- <cmd>` (v1.x) | Runs a command on each change in an ephemeral worktree, with results cached by `TreeId` | – | – | `fix`, branchless `test` |

**Hooks and signing.**
- `asc create`, `asc modify`, `asc reword`, `asc split` (once per piece) and `asc squash`/`asc fold` (for the combined message) run git's hooks at git's points (SPEC §9.4.1): `pre-commit` on the staged tree, `prepare-commit-msg`, `commit-msg`, `post-commit`. ASC's husky and commitlint therefore run. `--no-verify` skips `pre-commit` and `commit-msg`, as in git.
- Every rewrite, restack and `move` included, runs `pre-rebase` once up front (restack, move, reorder only) and `post-rewrite <amend|rebase>` once afterwards with all old/new pairs on stdin, plus `reference-transaction`. A restack does not run `pre-commit` or `commit-msg`, as `git rebase` does not. A non-zero hook aborts before any ref moves.
- Verbs sign as the matching git command does (`commit.gpgSign`, `-S`). The in-memory restack re-signs each rewritten commit when `commit.gpgSign=true` (through `gpg.program`, `gpg.format` and `user.signingkey`) and drops the signature when it is false, as `git rebase` does. A signing failure pauses the restack like a conflict (exit 1) and moves no ref for that change.
- Tests: S11 (hook order and abort with a husky and commitlint fixture) and S06 (re-sign on, drop on off, signing failure pauses).

**Stage 0 (git-remote mode) fallback.** A plain git remote carries no `m/` objects, so there are no server change ids or versions. Local verbs (`create`, `modify`, `restack`, `sl`, `undo`, `reword`, `abandon`, `split`, `squash`, `fold`, `move`) work on the local `ChangeMeta`. `asc sync` falls back to git semantics: fetch, then treat a change as landed when its commit is reachable from the trunk or its patch-id equals a trunk commit's patch-id since the merge base; squash merges are therefore reported as "cannot tell", not guessed. `asc submit`, `asc stack get`, `asc stack merge` and `asc share` need an asc server and exit 128 with `asc: <verb> needs an asc server (not available on a plain git remote)`.

**`--json` shapes** (every object carries `"version": 1`; ids are git SHA-1s, as in §11.1, plus the change id):

```jsonc
// asc sl --json  ==  Workspace.Stacks
{ "version": 1,
  "trunks": [{ "name": "master", "tip": "<sha>", "remote": "origin" }],
  "stacks": [{ "trunk": "master", "base": "<sha>", "behind": 3,
    "changes": [   // topological, bottom → top; build the tree from parent_change
      { "change": "<32 k-z>", "short": "<12>", "commit": "<sha>", "asc": "<64 hex>",
        "parent_change": null, "branches": ["jordan/parser"], "subject": "…",
        "author": { "name": "…", "email": "…", "time": 0 },
        "state": "draft|landed|obsolete|divergent", "needs_restack": false, "frozen": false,
        "empty": false, "current": true, "worktree": null,
        "review": { "number": 42, "url": "…", "state": "draft|open|merged|closed",
                    "approvals": 1, "checks": "pending|success|failure|none",
                    "queue": "none|queued|landing", "version": 3, "reviewed_version": 2 } } ] }] }

// every mutating verb
{ "version": 1, "op": "<op id>", "status": "ok|nothing|paused",
  "rewritten": [{ "change": "…", "old": "<sha>", "new": "<sha>" }],
  "refs": [{ "name": "refs/heads/…", "old": "<sha>", "new": "<sha>" }],
  "skipped": [{ "change": "…", "reason": "checked-out-elsewhere|frozen|conflict|merge|immutable" }],
  "conflicts": [{ "change": "…", "paths": ["…"] }],
  "reviews": [{ "change": "…", "number": 42, "url": "…", "created": false, "version": 4 }],   // submit, land
  "hints": ["…"] }

// asc op log --json
[{ "op": "…", "seq": 17, "at": 0, "ws": "…", "argv": ["git", "rebase", "master"],
   "refs": [{ "name": "…", "old": "…", "new": "…" }], "snapshot": null, "undone": false }]

// asc evolog --json
[{ "version": 3, "commit": "<sha>", "op": "amend|rebase|…", "at": 0, "actor": "…",
   "preds": ["<sha>"], "published": true }]
```

**Stack awareness in existing git commands: none (decision D4).**

| git command | Behaviour under `git` and `asc` |
|---|---|
| `commit --amend`, `rebase`, `rebase -i`, `pull --rebase` | Exactly git's. Descendants are untouched. Run `asc restack` to move them. |
| `rebase --update-refs`, `rebase.updateRefs` | git's own semantics (native, U27d) |
| every ref-moving command | Recorded in the op log, invisibly (`asc.opLog=false` turns it off) |
| every new commit | Gets a `ChangeMeta`, invisibly (SHAs unchanged) |
| `push`, `status`, `log`, `branch`, `branch -d`, `rev-parse` | Unchanged. Use `asc submit`, `asc sl`, `asc stack delete`, `asc change-id` |

Why there is no automatic restack after git rewrites: the checklist rule is that stack features never change what an existing git command does, and `asc commit` is git's `commit` (SPEC §11.1). The compat suite exercises both names, and agents use both. Automatic restack belongs to the new verbs, which always restack. If the user later wants an opt-in, it needs their explicit sign-off first.

---

## 4. Server and Forgejo

### 4.1 Model

| Concept | asc server (forge-neutral) | Forgejo fork |
|---|---|---|
| Change and versions | `changes` and `change_versions` (GC roots) | PR ↔ change (`asc_change` column) |
| Stack shape | `change_versions.parent_change`, plus `Changes.Stack` | `stack_parent_id` cache for listing; the truth is asc's |
| PR base | – | `base_branch` = the stack trunk (for example `master`) |
| PR diff base | – | `merge_base` = the head's first parent while the parent change is unlanded, else the merge base with trunk. Forgejo's diff, commits tab, review, "viewed" and outdated-comment logic all key off `merge_base`, so the patch and the commit list show only the change. |
| Review / approvals / inbox | `review_state` summary only, written by Forgejo | Owner |
| Landing | `UpdateRefs(LAND)` (atomic prefix), `land_map`, `FindLanding` | Merge button, queue, policy |
| Upstack restack | `RestackChanges` | Triggers it after a merge |

**Forgejo seams that D8 requires (all in S34/S36):**
- **merge_base recomputation.** Forgejo recomputes `merge_base` on every push and on base-branch updates (`UpdateCommitDivergence`/`getMergeBase`). S34 patches that path: when the PR has `asc_change` set, `merge_base` is the rule above and is never recomputed from the base branch.
- **Merge API.** `POST /repos/{o}/{r}/pulls/{n}/merge` (and the web merge button) on a PR with an unlanded parent PR returns 409 `stack-parent-unmerged` with a link to the stack merge. Otherwise the API would merge the whole prefix as one PR. S36 owns this block, and S38 owns "Merge stack (N)".

### 4.2 Flows

1. **Submit.** `asc submit` sends one `UpdateRefs` whose updates are `refs/for/<trunk>/asc-<change>`, bottom to top, plus push options `asc.draft` and `asc.reviewers`.
   - Pipeline step 4 ingests the closure once, including the `m/` objects.
   - Per ref, in order, asc calls Forgejo `proc-receive` (§13.5). The fork finds the PR by `(repo, change)`, not by `(topic, user)`. **Who may update it:** the PR's author, a repo admin or owner, or any user with write access when the PR has Forgejo's "allow edits from maintainers" ticked. Anyone else is rejected with `not allowed to update change <short>`; they submit their own change instead. This keeps a forged `preds` from replacing someone else's PR head. It creates the PR (title and body from the change's message) or records a new version, and sets `stack_parent_id` from the parent change's PR.
   - asc moves `refs/pull/<n>/head` with `SKIP_PRE` and appends `change_versions(v+1)`.
   - **Lease:** a version is accepted only if the server's latest version of that change is in the new changeset's `preds` closure, and every `m/` object in the push names (`meta.cs`) a changeset in the same closure (§2.1). Otherwise `CHANGE_DIVERGED` is returned, unless `--force` (which still needs the authorisation above).
2. **Review.** The PR shows the stack panel, "version N of M", interdiff against any earlier version, and "changes since your last review" (the version of the reviewer's last review `commit_id`). Comments outdate through Forgejo's existing line mapping.
3. **Land prefix 1..N.** Forgejo checks that every PR in the prefix is approved and green against the trunk's branch protection, then calls `UpdateRefs(master ← head(N), LAND, expect = the approved heads)`.
   - The whole prefix lands atomically (§8.5). `rewritten` gives each PR's landed changeset, and the PRs are marked merged.
   - Stacks always land rebase-style, one commit per change. A one-change PR's squash is the same thing.
4. **Restack after land.** Forgejo calls `RestackChanges(onto = new trunk tip, items = the open children bottom to top, with update_ref = refs/pull/<m>/head, atomic)`.
   - Each child gets a new version. The rewritten changesets use the **actor** of the call as committer (the user who merged, or who enabled merge-when-ready for the queue worker) and one **timestamp per call**, both carried in `RestackChangesRequest`, so one call is deterministic and a retry with the same request yields the same ids. The author and message are kept. `patch_unchanged` keeps approvals even with dismiss-stale enabled.
   - On conflict the child is marked "needs restack", and the author runs `asc sync && asc submit`.
5. **Queue.** "Merge when ready" on PR N enables it for 1..N.
   - A per-target-branch worker processes entries in enqueue order, one land in flight per branch, and always lands the largest ready prefix.
   - Admins can pause a branch: entries are accepted but not landed.
   - With `asc_stack_queue.revalidate = on_content_merge` (the default), the worker lands with the `LAND_NO_CONTENT_MERGE` flag. The server (U46/U48b amendment) fails the land with `LAND_NEEDS_CONTENT_MERGE` when §8.5 step 3 would need a content merge, instead of merging. The worker then restacks the stack as new versions and waits for checks again. Path-disjoint lands succeed straight away, and post-merge CI with `green/*` refs covers them (§12.6). `revalidate = off` omits the flag.
6. **Branch-chain PRs** (plain `git push` of branches, with base = another PR's head branch; S46) are recognised as stacked. When the parent merges, the base is retargeted to the parent's base, as GitHub does. They are not rebased on the server unless the author ticked "allow server restack".

### 4.3 RPCs (new `proto/asc/v1/changes.proto`, same port; additive `repo.proto` fields)

```proto
service Changes {
  rpc ResolveChanges(ResolveChangesRequest) returns (ResolveChangesResponse);  // cheap
  rpc Stack(StackRequest) returns (StackResponse);                             // cheap
  rpc Versions(VersionsRequest) returns (VersionsResponse);                    // cheap, paged
  rpc InterDiff(InterDiffRequest) returns (stream DiffResponse);               // expensive
  rpc Mergeability(MergeabilityRequest) returns (MergeabilityResponse);        // expensive, memoised
  rpc Overlaps(OverlapsRequest) returns (OverlapsResponse);                    // cheap (index)
  rpc RestackChanges(RestackChangesRequest) returns (RestackChangesResponse);  // mutating, x-asc-op-id
  rpc SetReviewState(SetReviewStateRequest) returns (SetReviewStateResponse);  // internal token only
  rpc RequestMerge(RequestMergeRequest) returns (RequestMergeResponse);        // mutating; proxies to the forge's queue
}
message ReviewState { string forge = 1; uint64 number = 2; string url = 3; string state = 4; bool draft = 5;
  uint32 approvals = 6; string checks = 7; string queue = 8; uint32 reviewed_version = 9; int64 updated = 10; }
message ChangeInfo { bytes change = 1; bytes head = 2; bytes git = 3; uint32 version = 4; bytes parent_change = 5;
  bool parent_current = 6; bytes landed_as = 7; ReviewState review = 8; repeated bytes visible = 9; /* >1 = divergent */ }
message ResolveChangesRequest { RepoRef repo = 1; repeated bytes changes = 2; repeated string revs = 3; }
message ResolveChangesResponse { repeated ChangeInfo changes = 1; }
message StackRequest { RepoRef repo = 1; string target = 2; oneof at { bytes change = 3; string rev = 4; uint64 review = 5; } }
message StackResponse { string trunk = 1; bytes base = 2; repeated ChangeInfo changes = 3; /* bottom → top, forks included */ }
message VersionsRequest { RepoRef repo = 1; bytes change = 2; string page_token = 3; }
message Version { uint32 v = 1; bytes cs = 2; bytes git = 3; bytes parent_change = 4; int64 at = 5; string actor = 6; }
message VersionsResponse { repeated Version versions = 1; string next_page_token = 2; }
message InterDiffRequest { RepoRef repo = 1; bytes change = 2; uint32 from_v = 3; uint32 to_v = 4; DiffFormat format = 5;
  repeated string paths = 6; }
message MergeabilityRequest { RepoRef repo = 1; string target = 2; repeated bytes changes = 3; }
message Mergeable { bytes change = 1; enum State { STATE_DISJOINT = 0; STATE_CLEAN_MERGE = 1; STATE_CONFLICTS = 2;
  STATE_NEEDS_RESTACK = 3; } State state = 2; repeated Conflict conflicts = 3; }
message MergeabilityResponse { repeated Mergeable results = 1; }
message OverlapsRequest { RepoRef repo = 1; string target = 2; bytes change = 3; repeated bytes paths = 4; }
message Overlap { bytes change = 1; ReviewState review = 2; repeated bytes paths = 3; }   // never includes a change of the same stack (its parent_change chain)
message OverlapsResponse { repeated Overlap overlaps = 1; }
message RestackItem { bytes change = 1; string update_ref = 2; RefTarget expect = 3; }
message RestackChangesRequest { RepoRef repo = 1; string onto = 2; repeated RestackItem items = 3; bool atomic = 4; bool dry_run = 5;
  string committer_name = 6; string committer_email = 7; int64 committer_time = 8; }   // set by the caller; defaults to the authenticated actor and the server clock
message RestackChangesResponse { repeated Rewrite rewritten = 1; repeated Conflict conflicts = 2; repeated bytes patch_unchanged = 3; }
message SetReviewStateRequest { RepoRef repo = 1; repeated bytes changes = 2; repeated ReviewState states = 3; }
message SetReviewStateResponse {}
message RequestMergeRequest { RepoRef repo = 1; bytes change = 2; bool now = 3; }   // bottom..change
message RequestMergeResponse { enum State { STATE_QUEUED = 0; STATE_LANDED = 1; STATE_REFUSED = 2; } State state = 1; repeated bytes pending = 2; string reason = 3; }
// The server forwards RequestMerge to the forge through the internal channel that carries proc-receive (§13.5), in the other direction.
// The CLI therefore never calls a forge API (D9). Forgejo answers from its queue (S39); with no forge attached, now=true lands directly and now=false is STATE_REFUSED.

// repo.proto, additive (hunks are amendments to U41, see §6)
//   ObjKind OBJ_KIND_CHANGE_META = 9;  LogEntry.change = 8;  LogEntry.stats = 9 (DiffStat);  LogEntry.meta = 10;
//   LogRequest.pickaxe = 15, pickaxe_regex = 16, with_stats = 17, scan_budget = 18 (candidates, default 10000);
//   LogResponse.truncated = N, resume_token = N+1 (set when the scan budget ran out);
//   UpdateRefs flag LAND_NO_CONTENT_MERGE; ErrorDetail.code += CHANGE_DIVERGED, CHANGE_FROZEN, LAND_NEEDS_CONTENT_MERGE, CHANGE_NOT_ALLOWED;
//   rpc CommitGraph(CommitGraphRequest) returns (stream CommitGraphResponse);  // §5; not the project graph of `asc graph`
//   rpc LineHistory(LineHistoryRequest) returns (stream LogResponse);          // §5; takes scan_budget
//   rpc Compare(CompareRequest) returns (CompareResponse);                     // §5
//   rpc RepoConfig(RepoConfigRequest) returns (RepoConfigResponse);            // autolinks, review.toml at a rev
```

Rate classes (§16.3): `ResolveChanges`, `Stack`, `Versions`, `Overlaps` and `RepoConfig` are `cheap`; `InterDiff`, `Mergeability` (on a miss), `RestackChanges`, `RequestMerge`, `CommitGraph` with stats, `LineHistory` and pickaxe `Log` are `expensive`.

### 4.4 Server tables and derived data

```sql
changes(ns BLOB, cs BLOB, change BLOB, meta BLOB, op SMALLINT, preds BLOB, PRIMARY KEY(ns, cs)) INDEX changes_by_change (ns, change)
change_versions(ns BLOB, change BLOB, v INT, cs BLOB, parent_change BLOB NULL, at BIGINT, actor TEXT, op_id BLOB,
                PRIMARY KEY(ns, change, v))                                  -- GC roots (§3.6), never in ListRefs
review_state(ns BLOB, change BLOB PRIMARY KEY2, state BLOB /*ReviewState*/, updated BIGINT)
open_paths(ns BLOB, path_key BLOB /*16 B, as path_changes*/, change BLOB, PRIMARY KEY(ns, path_key, change))
mergeable(ns BLOB, head BLOB, trunk_tip BLOB, state SMALLINT, conflicts BLOB, computed_at BIGINT, PRIMARY KEY(ns, head))   -- one row per head, overwritten
```

- `changes` is written in `UpdateRefs` step 4 and in `Ingest` for every new changeset whose closure carries a valid `m/` object (one row, eager; the validation of §2.1 applies). Land, `Merge(REBASE|SQUASH)` and `RestackChanges` write rows for the rewritten changesets in the same transaction as `land_map`.
- `landed_as(change)` is a lookup in `changes_by_change`, filtered to changesets on a land branch's `mainline` (one indexed join). It never scans messages (Scion lesson 23).
- `open_paths` is maintained when a version is published or a PR closes. The values are the `changes` keys of the head changeset itself, which are relative to its parent, so no diff is needed. `Overlaps` costs O(paths × matches) and skips changes of the same stack (found through the `parent_change` chain).
- `mergeable` is a plain SQL table, not a keyed derived type, so it cannot grow with trunk history: one row per head, overwritten when the head or the trunk tip changes, and deleted when the PR closes. It holds `{state, conflicts}` and uses the §4.5 path-disjointness check first, reading content only for intersecting paths. Entries are recomputed lazily on request. Forgejo asks only for open PRs whose `open_paths` intersect the new trunk changes.
- The derived type `diffstat.v1` (lazy, per changeset against `parents[0]`) holds `Vec<(path, added, removed)>`. It feeds `CommitGraph` stats, visual file history and `Log.with_stats`.

### 4.5 Forgejo UI (all rendered from the RPCs above)

| Surface | Content |
|---|---|
| PR page | A stack navigator (bottom to top; state, checks and approvals per PR; keys to move between them), a "needs restack" banner with a server-restack button, "Merge stack (N)" or the mid-stack block, a version dropdown with interdiff, "since my last review", and overlap and mergeability badges |
| Repo | A commit graph page over `CommitGraph`; blame and file history (existing pages, via ascbackend); a stack compare view (`Compare` between two versions or two stacks) |
| Inbox `/pulls/inbox` and `GET /api/v1/user/asc/inbox` | Sections: needs your review, returned to you, ready to merge, blocked, waiting, drafts, queued or recently merged. Whose turn is derived from review requests, the latest review and the version after it. Per-user pin and timed snooze. ASC's plugins call this API directly. |
| Actions | The context gains `asc_change`, `asc_stack_position`, `asc_stack_size` and `asc_stack_parent_sha`. Workflows run `asc affected --base $asc_stack_parent_sha` to test only the change's own projects. |
| Undo-aware operations | "Restore version v" (moves `refs/pull/<n>/head` back to a kept version); "Revert landed stack" (`Apply(REVERT)` over the `FindLanding` results, newest first) |

---

## 5. Plugin API surface

All of these are additive. They are served by the daemon over IPC (`workspace.proto`), by the server where noted, and by `asc … --json`.

| Need | API | Served from | Efficiency bound |
|---|---|---|---|
| Inline, hover, status-bar and gutter blame; heatmap; CodeLens | `Workspace.Blame` plus `buffer` (unsaved bytes) and `ignore_whitespace`; `BlameResponse.commits[].change`; parallel `reviews[]` | `blame.v1` memo (§4.6) with path-index candidates. Buffer lines are mapped by one line diff against the `HEAD` blob. | O(file) per request after the first derive. A warm memo is never re-walked. |
| File history (renames followed) | `Workspace.Log` / `Repo.Log` with `paths`, `follow`, `with_stats` | `path_changes` on land branches; otherwise a gen-pruned walk over `changes` keys (`pathlog` derived type when measured) | O(result + log n) on land branches (§5.1) |
| Line history (`-L`) | `LineHistory(rev, path, start, end, limit, scan_budget)` | Candidates from the path index; the range is carried backwards through each candidate's single-file line diff and stops when it is empty. The scan stops after `scan_budget` candidates (default 10,000) and returns `truncated` plus a resume token. | O(min(candidates touching the path, scan_budget) × file size) |
| Commit graph, minimap, filters | `Repo.CommitGraph` / `Workspace.CommitGraph(include, exclude, preset: all\|current\|smart\|stack, lanes, stats, page_token)` (named apart from `asc graph`, the project graph of SPEC §12A.9). Rows carry `LogEntry`, refs, `column`, `edges`, `stats`, `change`, `review`. The workspace variant adds a WIP row per worktree. | Generation-ordered pages. Lanes come from the deterministic `asc-engine::commit_graph::lanes`, whose active-lane state rides in the page token. The state is capped at 256 lanes: further lanes share one `overflow` column and the token stays under 4 KiB. | O(page) per request. Earlier pages are never re-read. Stats come from `diffstat.v1`. |
| Commit details | `ReadChangeset` plus `ResolveChanges` | existing plus the change index | `cheap` |
| Branch comparison (ahead/behind, files, two or three dots) | `Compare(a, b, three_dot, limit)` returning `{merge_base, ahead, behind, ahead_page, behind_page, files}` | `MergeBase` with skip lists; mainline positions on land branches; `Diff(NAMES)` | O(log n + result) |
| Worktrees view and multi-WIP | `Workspace.Worktrees` returning `{ws_id, path, mount, mode, head_ref, head, dirty, staged, ahead, behind, ephemeral_until, locked, in_progress}` | `workspaces` rows plus each journal's dirty count | O(workspaces + changed paths); no tree walk |
| Worktree management | `asc worktree add\|remove\|lock --json` (existing verbs) | §9.3 | `add` < 1 s, independent of repo size |
| Stacks / smartlog | `Workspace.Stacks` (= `asc sl --json`) | §2.2, local | O(heads × stack length × log n) |
| Launchpad / PR triage | Forgejo `GET /api/v1/user/asc/inbox` (not an asc RPC) | Forgejo DB plus `review_state` | Forgejo indexed queries |
| Conflicts (merge editor) | `Workspace.Conflicts` returning `{path, kind, base, ours, theirs, marked}` | staging stages 1–3 (§9.5) | O(conflicted paths) |
| Graph actions, rebase editor, drag and drop | The CLI verbs with `--json` / `--dry-run --json` (plan and predicted conflicts) | `merge_trees` in memory; `Rebase(dry_run)` remotely | O(changed paths) unless paths intersect |
| Autolinks, web URLs | `RepoConfig(rev)` returning `autolinks[{prefix, url}]`; the `web` template in `/.well-known/asc` | `.asc/review.toml` at the revision | `cheap`, cached by `FileId` |
| Events | `Watch` adds `KIND_STACK` (S49) (a change rewritten or restacked, a review state changed) | the op log and `review_cache` refresh | push-based, no polling |

---

## 6. Proposed PLAN units

These follow the PLAN conventions: small units, disjoint owned files, one acceptance command each. Stack verbs live under `crates/asc/src/stack/`. Every unit owns its own test file (`crates/asc/tests/stack_<name>.rs`); no test file is shared. S07 owns only `crates/asc/src/stack/cli/mod.rs`, which uses `automod::dir!` and a `linkme` distributed slice, so each verb unit adds its own file and registers itself without touching `mod.rs`. Unit ids are stable: S16 is withdrawn and not reused.

**Amendments to existing units.** These files belong to the unit named. The S-unit makes the hunk and is added to that unit's Owns list as a recorded amendment.

| Existing unit and file | Hunk | Made by |
|---|---|---|
| U20 `crates/asc/src/ext.rs` | Define `RewriteSink { on_rewrite(old, new, op) }`, `OpSink { begin(argv) -> OpId, end(OpId), snapshot_wc() }` with no-op defaults | S04 |
| U22 (commit, amend), U27a (merge, cherry-pick, revert), U27c (rebase), U27d (`rebase -i`), U27e (stash), U32 (`pull --rebase`) | One sink call at each place a changeset is created or rewritten | S04 |
| U21d (`reset`, `restore`), U23 (`checkout`, `switch`) | `snapshot_wc()` before a working-copy-destroying path (§2.3) | S04 |
| U33 `crates/asc-git/src/import.rs` | Call `import_change::write_meta` for each imported commit | S45 |
| U41 `proto/asc/v1/repo.proto` | The additive `repo.proto` hunks of §4.3 | S17 |
| U73 `proto/asc/v1/workspace.proto`, `docs/plugin-api.md` | The additive `workspace.proto` hunks of §5, and a link to `docs/plugin-api-stack.md` | S31 |
| U46 / U48b / U68 (`UpdateRefs(LAND)`, merge styles, land content merge) | The `LAND_NO_CONTENT_MERGE` flag and `LAND_NEEDS_CONTENT_MERGE` error | S52 |

| ID | Title | Crate / repo | Depends | Owns | Accept |
|---|---|---|---|---|---|
| S01 | `ChangeId`, content-addressed `ChangeMeta`, display, import derivation | asc-core | U04 | `crates/asc-core/src/change.rs`, `crates/asc-core/tests/golden/change.txt` | `cargo test -p asc-core change::` |
| S02 | Local change index and rewrite recording | asc-state, asc-engine | S01, U17, U26 | `crates/asc-state/migrations/0201_changes.sql`, `crates/asc-state/src/changes.rs`, `crates/asc-engine/src/change.rs` | `cargo test -p asc-state changes:: && cargo test -p asc-engine change::` |
| S03 | Operation log store, pruning, argv redaction | asc-state | U17, U09 | `crates/asc-state/migrations/0202_oplog.sql`, `crates/asc-state/src/oplog.rs` | `cargo test -p asc-state oplog::` |
| S04 | `RewriteSink`/`OpSink` implementations for git commands (ids, op log, pre-destroy snapshots; invisible) | asc | S02, S03, U20, U21d, U22, U23, U27a, U27c, U27d, U27e, U32 | `crates/asc/src/stack/sinks.rs`, `crates/asc/tests/stack_sinks.rs` | `cargo test -p asc --test stack_sinks && cargo test -p asc-compat --test compat` |
| S05 | Stack model and revset subset (`-r/--revset` only) | asc-engine | S02, U13, U14b | `crates/asc-engine/src/stack/{mod.rs,revset.rs}` | `cargo test -p asc-engine stack::` |
| S06 | In-memory restack planner and executor (with re-signing) | asc-engine | S05, U26 | `crates/asc-engine/src/stack/restack.rs` | `cargo test -p asc-engine stack::restack` |
| S07 | Stack CLI frame, the D5 precedence rule (incl. autocorrect), `--json` types | asc | S05, U20, U20a | `crates/asc/src/stack/{mod.rs,precedence.rs,json.rs}`, `crates/asc/src/stack/cli/mod.rs`, `crates/asc/tests/stack_frame.rs` | `cargo test -p asc --test stack_frame` |
| S08 | `asc sl`, `asc stack info\|parent\|children\|trunk`, `asc change-id` | asc | S07, U24a | `crates/asc/src/stack/cli/{sl.rs,info.rs,change_id.rs}`, `crates/asc/tests/stack_sl.rs` | `cargo test -p asc --test stack_sl` |
| S09 | Navigation `up/down/top/bottom/next/prev` (exit 2 at a fork) | asc | S07, U23 | `crates/asc/src/stack/cli/nav.rs`, `crates/asc/tests/stack_nav.rs` | `cargo test -p asc --test stack_nav` |
| S10 | `asc restack`, `asc continue`, `asc abort` | asc | S06, S07, U27c | `crates/asc/src/stack/opstate.rs`, `crates/asc/src/stack/cli/{restack.rs,continue_.rs,abort.rs}`, `crates/asc/tests/stack_restack.rs` | `cargo test -p asc --test stack_restack` |
| S11 | `asc create`, `asc modify`, `asc reword` (hooks) | asc | S10, U22 | `crates/asc/src/stack/cli/{create.rs,modify.rs,reword.rs}`, `crates/asc/tests/stack_create.rs` | `cargo test -p asc --test stack_create` |
| S12 | `asc absorb` | asc-engine, asc | S10, U21c, U28a | `crates/asc-engine/src/stack/absorb.rs`, `crates/asc/src/stack/cli/absorb.rs`, `crates/asc/tests/stack_absorb.rs` | `cargo test -p asc-engine stack::absorb && cargo test -p asc --test stack_absorb` |
| S13 | `asc split`, `asc squash`, `asc fold` | asc | S10, U21c | `crates/asc/src/stack/cli/{split.rs,squash.rs,fold.rs}`, `crates/asc/tests/stack_split.rs` | `cargo test -p asc --test stack_split` |
| S14 | `asc move`, `asc reorder`, `asc stack rename\|delete\|pop` | asc | S10, U27d | `crates/asc/src/stack/cli/{move_.rs,reorder.rs,lifecycle.rs}`, `crates/asc/tests/stack_move.rs` | `cargo test -p asc --test stack_move` |
| S15 | `asc undo/redo/op` | asc | S03, S04, S07, U23 | `crates/asc/src/stack/cli/{undo.rs,op.rs}`, `crates/asc/tests/stack_undo.rs` | `cargo test -p asc --test stack_undo` |
| S16 | Withdrawn: opt-in `restackOnRewrite` (see Review notes) | – | – | – | – |
| S17 | Proto: `changes.proto` and conversions, plus the `repo.proto` amendment | asc-rpc | S01, U41 | `proto/asc/v1/changes.proto`, `crates/asc-rpc/src/changes.rs` | `cargo test -p asc-rpc changes::` |
| S18 | Server change index, `m/` ingest and validation, versions, GC roots | asc-serve | S17, U43b, U66 | `crates/asc-serve/src/changes/{mod.rs,index.rs,versions.rs}`, `crates/asc-serve/migrations/{postgres,sqlite}/0201_changes.sql` | `cargo test -p asc-serve changes::index changes::versions` |
| S19 | `ResolveChanges`, `Stack`, `Versions`, `SetReviewState` | asc-serve | S18, U47 | `crates/asc-serve/src/changes/rpc.rs` | `cargo test -p asc-serve changes::rpc` |
| S20 | `InterDiff` | asc-engine, asc-serve | S19, U26 | `crates/asc-engine/src/interdiff.rs`, `crates/asc-serve/src/changes/interdiff.rs` | `cargo test -p asc-engine interdiff:: && cargo test -p asc-serve changes::interdiff` |
| S21 | `Mergeability` (one row per head), `Overlaps`, `open_paths` | asc-serve | S19, U65 | `crates/asc-serve/src/changes/{mergeable.rs,overlaps.rs}`, `crates/asc-serve/migrations/{postgres,sqlite}/0202_open_paths.sql` | `cargo test -p asc-serve changes::mergeable changes::overlaps` |
| S22 | `RestackChanges` (actor committer, one timestamp per call) | asc-serve | S19, U68 | `crates/asc-serve/src/changes/restack.rs` | `cargo test -p asc-serve changes::restack` |
| S23 | Client transport of change metadata (push closure `m/`, fetch `LogEntry.change/meta`) | asc-engine | S02, S17, U31 | `crates/asc-engine/src/sync/changes.rs` | `cargo test -p asc-engine sync::changes` |
| S24 | `asc submit` | asc | S19, S23, U32, U50, U44 | `crates/asc/src/stack/cli/submit.rs`, `crates/asc/tests/stack_submit.rs` | `cargo test -p asc --test stack_submit` |
| S25 | `asc sync` (with the Stage 0 fallback) | asc | S10, S19, S23, U32, U39, U66 | `crates/asc/src/stack/cli/sync.rs`, `crates/asc/tests/stack_sync.rs` | `cargo test -p asc --test stack_sync` |
| S26 | `asc stack get\|freeze\|unfreeze` (`--worktree` opens a plain worktree) | asc | S19, S23, U54w | `crates/asc/src/stack/cli/get.rs`, `crates/asc/tests/stack_get.rs` | `cargo test -p asc --test stack_get` |
| S27 | `asc evolog`, `asc interdiff` | asc | S20, S07 | `crates/asc/src/stack/cli/evolog.rs`, `crates/asc/tests/stack_evolog.rs` | `cargo test -p asc --test stack_evolog` |
| S28 | `asc stack merge` (calls `Changes.RequestMerge`) | asc | S24, S50 | `crates/asc/src/stack/cli/merge.rs`, `crates/asc/tests/stack_merge.rs` | `cargo test -p asc --test stack_merge` |
| S29 | `CommitGraph` RPC with capped lanes, `diffstat.v1` | asc-engine, asc-serve | U47, U14, U15 | `crates/asc-engine/src/commit_graph/lanes.rs`, `crates/asc-engine/src/derive/diffstat.rs`, `crates/asc-serve/src/commit_graph_rpc.rs` | `cargo test -p asc-engine commit_graph::lanes derive::diffstat && cargo test -p asc-serve commit_graph_rpc::` |
| S30a | `LineHistory` RPC with scan budget | asc-engine, asc-serve | U47, U65 | `crates/asc-engine/src/history/linehist.rs`, `crates/asc-serve/src/history/linehist_rpc.rs` | `cargo test -p asc-engine history::linehist && cargo test -p asc-serve history::linehist_rpc` |
| S30b | `Compare` RPC | asc-engine, asc-serve | U47, U65 | `crates/asc-engine/src/history/compare.rs`, `crates/asc-serve/src/history/compare_rpc.rs` | `cargo test -p asc-engine history::compare && cargo test -p asc-serve history::compare_rpc` |
| S30c | Pickaxe `Log` (path required, scan budget) | asc-engine, asc-serve | U47, U65 | `crates/asc-engine/src/history/pickaxe.rs`, `crates/asc-serve/src/history/pickaxe_rpc.rs` | `cargo test -p asc-engine history::pickaxe && cargo test -p asc-serve history::pickaxe_rpc` |
| S30d | `RepoConfig` RPC (autolinks, `review.toml` at a rev) | asc-serve | U47 | `crates/asc-serve/src/history/repo_config.rs` | `cargo test -p asc-serve history::repo_config` |
| S31 | Workspace plugin additions (§5) | asc | U73, S08, S29, S30a, S30b, S30c, S30d | `crates/asc/src/daemon/plugin_api_stack.rs`, `docs/plugin-api-stack.md`, `crates/asc/tests/plugin_api_stack.rs` | `cargo test -p asc --test plugin_api_stack` |
| S32 | `asc share` (rooted by `refs/snapshots/<id>` with expiry) | asc | S23, U48a | `crates/asc/src/stack/cli/share.rs`, `crates/asc/tests/stack_share.rs` | `cargo test -p asc --test stack_share` |
| S33 | `asc stack exec` (v1.x) | asc | S06, U54w | `crates/asc/src/stack/cli/exec.rs`, `crates/asc/tests/stack_exec.rs` | `cargo test -p asc --test stack_exec` |
| S34 | PR ↔ change, `stack_parent_id`, the `merge_base` rule and its recomputation patch, AGit lookup by change, update authorisation | forgejo | K09, K12, S19 | `models/issues/pull_asc_stack.go`, `models/forgejo_migrations/v*_asc_stack.go`, `services/agit/agit_asc.go`, `services/pull/asc_stack_base.go` | `go test ./services/pull/... ./services/agit/... -run TestAscStack` |
| S35 | `SetReviewState` notifier on PR, review and check events | forgejo | S34 | `services/ascsync/review_state.go` | `go test ./services/ascsync/... -run TestAscReviewState` |
| S36 | Stack panel, needs-restack banner, mid-stack merge block (web and `POST /pulls/{n}/merge`) | forgejo | S34 | `routers/web/repo/pull_asc_stack.go`, `routers/api/v1/repo/pull_asc_merge_guard.go`, `templates/repo/pulls/asc_stack.tmpl` | `go test ./routers/... -run TestAscStackPanel\|TestAscMergeGuard` |
| S37 | Versions dropdown, interdiff, since-last-review | forgejo | S34, S20 | `services/pull/asc_versions.go`, `templates/repo/pulls/asc_versions.tmpl` | `go test ./services/pull/... -run TestAscVersions` |
| S38 | Merge stack (N), post-merge `RestackChanges`, approvals kept | forgejo | S36, S22 | `services/pull/asc_stack_merge.go` | `go test ./services/pull/... -run TestAscStackMerge` |
| S39 | Stack queue: merge-when-ready cascade, order, pause, revalidate, the `RequestMerge` internal endpoint | forgejo | S38, S50, S52 | `services/automerge/asc_stack_queue.go`, `routers/private/asc_request_merge.go`, `models/forgejo_migrations/v*_asc_queue.go` | `go test ./services/automerge/... ./routers/private/... -run TestAscStackQueue\|TestAscRequestMerge` |
| S40 | Actions stack context (K14 extension) | forgejo | S34, K14 | `services/actions/asc_stack_context.go` | `go test ./services/actions/... -run TestAscStackContext` |
| S41a | Inbox state model (whose turn, pin, snooze) | forgejo | S35 | `models/issues/asc_inbox_state.go` | `go test ./models/issues/... -run TestAscInboxState` |
| S41b | Inbox API `GET /api/v1/user/asc/inbox` | forgejo | S41a | `routers/api/v1/user/asc_inbox.go` | `go test ./routers/api/v1/user/... -run TestAscInbox` |
| S41c | Inbox page | forgejo | S41b | `routers/web/user/asc_inbox.go`, `templates/user/asc_inbox.tmpl` | `go test ./routers/web/user/... -run TestAscInboxPage` |
| S42 | Commit graph page over `CommitGraph` | forgejo | S29, K10 | `routers/web/repo/asc_graph.go`, `templates/repo/asc_graph.tmpl` | `go test ./routers/web/repo/... -run TestAscGraph` |
| S43 | Overlap and mergeability badges, stack compare view | forgejo | S21, S30b, S36 | `routers/web/repo/asc_compare.go`, `templates/repo/pulls/asc_badges.tmpl` | `go test ./routers/web/repo/... -run TestAscOverlap` |
| S44 | Suggested changesets (v1.x) | forgejo, asc-serve | S37, U48a | `services/pull/asc_suggest.go`, `templates/repo/pulls/asc_suggest.tmpl` | `go test ./services/pull/... -run TestAscSuggest` |
| S45 | Git import writes `ChangeMeta` (jj header, Gerrit trailer, scoped id) | asc-git | S01, S02, U33 | `crates/asc-git/src/import_change.rs`, `crates/asc-git/tests/import_change.rs` | `cargo test -p asc-git --test import_change && cargo test -p asc-git --release --test asc_roundtrip -- --ignored` |
| S46 | Branch-chain PR retarget (§4.2 flow 6) | forgejo | S34 | `services/pull/asc_branch_chain.go` | `go test ./services/pull/... -run TestAscBranchChain` |
| S47 | "Restore version" (moves `refs/pull/<n>/head` to a kept version) | forgejo | S37 | `services/pull/asc_restore_version.go` | `go test ./services/pull/... -run TestAscRestoreVersion` |
| S48 | "Revert landed stack" (`Apply(REVERT)` over `FindLanding`, newest first) | forgejo | S38, U66 | `services/pull/asc_revert_stack.go` | `go test ./services/pull/... -run TestAscRevertStack` |
| S49 | `Watch KIND_STACK` events (server emit, client delivery) | asc-serve, asc | S19, S02, U73 | `crates/asc-serve/src/changes/watch.rs`, `crates/asc/src/daemon/watch_stack.rs`, `crates/asc/tests/stack_watch.rs` | `cargo test -p asc-serve changes::watch && cargo test -p asc --test stack_watch` |
| S50 | `RequestMerge` RPC (proxies to the forge's queue) | asc-serve | S19, U46 | `crates/asc-serve/src/changes/request_merge.rs` | `cargo test -p asc-serve changes::request_merge` |
| S51 | `asc uncommit`, `asc unamend`, `asc abandon` | asc | S10, S11 | `crates/asc/src/stack/cli/{uncommit.rs,abandon.rs}`, `crates/asc/tests/stack_abandon.rs` | `cargo test -p asc --test stack_abandon` |
| S52 | Server `LAND_NO_CONTENT_MERGE` flag | asc-serve | U46, U48b, U68, S17 | `crates/asc-serve/tests/land_no_content_merge.rs` | `cargo test -p asc-serve --test land_no_content_merge` |

Required tests (each unit's brief):
- **S01:** golden ids; an envelope round trip; `put` of a tampered `m/` object fails; k–z display and parse.
- **S03:** argv redaction (URL credentials, `-c` values, `--token`); pruning bounds.
- **S04:** the whole compat suite is unchanged, byte for byte; `reset --hard`, `checkout -f` and `switch --discard-changes` each leave a snapshot that `asc undo` restores; `clean -f` prints the gap hint.
- **S05/S06:** the stack equals a naive oracle on random DAGs; a stack of 50 changes restacks with 0 content reads when paths are disjoint; with `commit.gpgSign=true` every rewritten commit is signed, with it false none is, and a signing failure pauses with exit 1 and moves no ref. S05: `immutable` evaluation issues no tag lookups; a Gerrit-style shared id on two immutable commits is neither obsolete nor divergent.
- **S07:** `help.autocorrect=immediate` does not turn a typo of a git command into a stack verb, and a stack verb name is not autocorrected.
- **S11:** `asc create`/`modify`/`reword` run `pre-commit`, `commit-msg` and `post-commit` (husky and commitlint fixture; a rejecting hook moves no ref); `post-rewrite` runs once per restack with all pairs.
- **S12:** absorb targets equal `git absorb`/Sapling on fixtures.
- **S15:** undo of `git rebase`, `asc modify` and `asc split` restores refs, `HEAD` and the working copy.
- **S17/S18:** a meta whose `cs` is not in the push closure is rejected; `landed_as` uses no sequential scan (`EXPLAIN`); `CHANGE_DIVERGED` is returned on a concurrent rewrite.
- **S21:** after the trunk moves ten times the `mergeable` table still has one row per head; `Overlaps` omits the requested change's own stack.
- **S22:** `patch_unchanged` holds for metadata rebases; two calls with the same request produce identical changeset ids.
- **S24:** one `UpdateRefs` call per submit; PRs are created bottom-up.
- **S25:** on a plain git remote, `sync` detects landed changes by ancestry and patch-id, and `submit`, `get` and `stack merge` exit 128.
- **S29:** lanes are identical for one page of 1,000 rows and for ten pages of 100; a graph with 1,000 live lanes yields a page token under 4 KiB.
- **S30a/S30c:** a request that exhausts `scan_budget` returns `truncated` with a resume token; pickaxe without a path is rejected.
- **S34:** a user without write access, or a non-author without "allow edits", cannot update a PR by forging `preds`; a push does not change a stacked PR's `merge_base`; the commits tab lists only the change.
- **S36:** `POST /pulls/{n}/merge` on a mid-stack PR returns 409.
- **S38/S39:** a partial merge lands exactly 1..N atomically; the simulation (`asc-sim`) lands 20 stacks concurrently with no lost or duplicated change.
- **S52:** a land that would need a content merge fails with `LAND_NEEDS_CONTENT_MERGE` and changes no ref.

---

## 7. Edits needed in SPEC.md and PLAN.md (not made here)

- **§0 and §19:** replace "design pending" with decisions D1–D12, and drop §19 item 7.
- **§2.2, §3.1 and §7 `ObjKind`:** envelope kind 6 `ChangeMeta`, key prefix `m/<meta id>` (content-addressed, so SPEC §0 decision 5 holds unchanged), `OBJ_KIND_CHANGE_META = 9`. §3.6 GC roots gain `change_versions`, and `m/` objects follow their changeset.
- **§5:** add the type `diffstat.v1`. **§8.2:** add the tables `changes`, `change_versions`, `review_state`, `open_paths` and `mergeable`. **§9.2:** add `changes`, `change_preds`, `op_log`, `stack_frozen` and `review_cache`.
- **§7:** add the `Changes` service (including `RequestMerge`) and the additive `repo.proto`/`workspace.proto` fields of §4.3 and §5, with `CommitGraph` named apart from `asc graph`. **§16.3:** add the rate classes of §4.3.
- **§11.1 config:** document the `asc.stack.*` keys. **§11.2:** list the stack verbs in the asc-only commands paragraph, including `asc change-id` and `asc stack merge` (next to the existing `asc landed`). **§11.4:** add the D5 precedence rule for argv[0] `git`, including `help.autocorrect`. **§11.5:** no new git flag (`rev-parse --change-id` was dropped).
- **§12.6:** clarify that the stack queue is ordered merge-when-ready over `LAND`, with no speculative CI and no global lock. This does not contradict "no merge queue".
- **§13.3:** add S34–S44 to the Forgejo patch stack (after K17). **§13.5:** AGit topic `asc-<change>`, and PR lookup by change.
- **§17.1:** add a compat case asserting that op-log recording, pre-destroy snapshots and `ChangeMeta` leave git output unchanged. Stack verbs get their own tests (`stack_*`), not compat cases.
- **PLAN:** add the S-units to the index with the dependencies above, and record the amendments table of §6 on U20, U21d, U22, U23, U27a, U27c, U27d, U27e, U32, U33, U41, U46, U48b, U68 and U73. Stack work starts after U26/U27c (local) and U47/U68 (server).
- **`scion-checklist.md` (not edited here):** D8 needs the user's sign-off against the recorded line "PR base is the parent, retargeted when the parent merges"; once given, add it there.

---

## Review notes

Points of the 2026-10-22 design review that were rejected or only partly applied:

- **#5 (opt-in `restackOnRewrite`):** applied by deleting the opt-in and S16, not by asking for sign-off, because the checklist has no exception for opt-ins. Re-adding it needs the user's explicit sign-off.
- **#6 (D8 vs the checklist):** I cannot record a user decision that has not been given. D8 is now marked as a storage-level deviation that needs sign-off (see D8 and §7), and the Forgejo seams (merge API block, `merge_base` patch) are specified in §4.1.
- **#2 (`m/` unverifiable):** applied as the content-addressed option, not the "unverified class" option, so SPEC §3 needs no new tier rules.
- **#7 (revset `~`):** applied as the "only under `-r/--revset`" option, not by changing operators, so `~` keeps jj's difference meaning inside a revset.
- **#8 (`asc land`):** renamed to `asc stack merge` rather than documenting two near-identical names; `asc landed` is unchanged.
- **#15 (virtual worktree):** applied by saying "plain" and noting that a virtual worktree follows U57/U58, rather than adding U57/U58 as dependencies of S26.
