# First-class conflicts

Status: design, decided 2026-11-13 (roadmap row 16, docs/ROADMAP-NEXT.md). Nothing here is built.
It replaces the "jj-style first-class conflicts" entry of the out-of-scope list in SPEC §0.

## Why

A restack or a land stops at the first change that does not merge cleanly. An agent that works
unattended then holds a paused operation, a stack it cannot move and a choice it cannot make. A
person resolves the conflict hours later, or the agent guesses.

With first-class conflicts a change can be stored in a conflicted state. A restack rewrites every
change of the stack, records the conflicts it meets inside the changes that have them, and
finishes. The conflict is resolved later, by a person or by an agent, in the change that holds it.
The resolution then flows into every descendant on the next restack. A conflicted change can never
land on a shared ref, so trunk never holds one.

jj and Sapling showed the model. This design takes jj's algebra (a conflict is a list of terms
that add and remove) and keeps asc's frozen formats and git surface.

Non-goals:

- Changing the v1 tree format, the changeset envelope version or any existing object id (SPEC §0
  decision 3). Every object stored today keeps its bytes and its id.
- Changing git-surface commands. `git rebase`, `git merge`, `git cherry-pick` and `git commit`
  through the shim keep git's behaviour, stages and messages (SPEC §0 decision 6).
- Landing a conflicted change anywhere that other people or CI treat as truth.

## Current behaviour

A conflict is a return value today, never a stored state. These are the places where it stops
work:

- `crates/asc-engine/src/rebase.rs:139-158`: `rebase_one`: an intersecting path that matches
  `no_merge` globs gives `Rebased::Conflicts` with `marked_id: None`.
- `crates/asc-engine/src/rebase.rs:161-165`: `rebase_one`: `merge_trees` is not clean, so it returns
  `Rebased::Conflicts(outcome.conflicts)`. Nothing is stored except the marked files.
- `crates/asc-engine/src/rebase.rs:202-232`: `rebase_stack`: after the first conflict `onto` is
  `None`, and every later item is `Error::Blocked`.
- `crates/asc-engine/src/stack/restack.rs:227-241`: `rewrite_step` maps `Rebased::Conflicts` to
  `Err(conflicts)`.
- `crates/asc-engine/src/stack/restack.rs:267-276`: `execute_restack` returns `Outcome::Paused {
  cause: Pause::Conflicts(c) }`; no ref moves.
- `crates/asc/src/stack/cli/restack.rs:495-510`: The CLI replays the paused change into the work
  tree, writes `opstate`, and exits 1 with "Resolve the conflicts ... then run 'asc continue'" (line
  565).
- `crates/asc-engine/src/land.rs:213-233`: `content_merge_land` stops at the first changeset that
  conflicts (`out.conflicts = conflicts`, line 229).
- `crates/asc-serve/src/land.rs:172-185`: The rebase strategy answers `LAND_CONFLICT` (or
  `NeedsContentMerge`) with at most `MAX_CONFLICT_PATHS` (100) paths.
- `crates/asc-serve/src/land_merge.rs:123-132`: The content-merge strategy answers `LAND_CONFLICT`
  with `wire::Conflict` records (diff3 `marked_id`).
- `crates/asc-serve/src/changes/restack.rs:349`: `RestackChanges` and `Stacks.Restack` report
  `Conflict(...)`; the item and every item above it stay as they were
  (`RESTACK_ITEM_STATE_CONFLICT`, `RESTACK_ITEM_STATE_BLOCKED`).

The data that a conflict needs already exists in memory. `merge::Conflict`
(`crates/asc-engine/src/merge/mod.rs:246`) holds the path, a `ConflictKind`, the `Stages` (base,
ours and theirs `Entry`), the `marked_id` of the file with markers, and `from` for a file/directory
conflict. `MergeOutcome.root` already holds the marked file at the conflicted path. The proto
`Conflict` (`proto/asc/v1/repo.proto:168`) carries the same fields. What is missing is a place to
store the record, rules that keep it correct, and gates that keep it off trunk.

Two gaps would open as soon as a conflicted changeset could exist, and the design closes both:

- A land with no rewrite does not merge. `land::plan` (`crates/asc-serve/src/land.rs:261-268`)
  fast-forwards when the branch is absent or the stack sits on the tip, and a plain
  `FAST_FORWARD` push to trunk never reaches `content_merge_land`. The gate must refuse there.
- Ingest refuses unknown `extra` keys (`crates/asc-serve/src/ingest.rs:954`,
  `crates/asc-engine/src/commit.rs:416`). That is what makes old servers and old clients fail
  closed.

## Data model

### Terms and the conflict value

A path's value in a change is either one term (an ordinary entry, or absent) or a conflict. A
conflict is an alternating sum, as in jj:

```
value = adds[0] - removes[0] + adds[1] - removes[1] + ... + adds[n]
```

`adds` has exactly one more element than `removes`. A plain two-sided conflict is
`adds = [ours, theirs]`, `removes = [base]`. Rebasing a value `c` from parent value `p` onto new
parent value `p2` is `p2 - p + c`, flattened into one sum. Simplification removes a pair where a
remove equals an add (scanning `removes` in order, each cancelling the first equal add). A sum with
one add left, or with all adds equal, is resolved.

### Stored types (crate `asc-core`, new module `crates/asc-core/src/conflict.rs`)

```rust
pub struct ConflictSet { pub entries: Vec<(RepoPath, Conflicted)> } // sorted map, non-empty
pub struct Conflicted {
    pub kind: ConflictKind,       // Content|AddAdd|ModifyDelete|FileDir|Binary, in that order
    pub adds: Vec<Term>,          // 2..=MAX_CONFLICT_SIDES; adds[0] is the destination side
    pub removes: Vec<Term>,       // adds.len() - 1
    pub shown: Shown,             // what the root tree holds at the path
    pub from: Option<RepoPath>,   // FileDir only: where the file was before the ort-style move
}
pub enum Term { Absent, Leaf { kind: Kind /* File|Exec|Symlink|Gitlink */, id: [u8; 32],
                               size: u64, git: GitOid } }
pub enum Shown { Markers { marker_len: u8 }, Side { index: u8 } }
```

`ConflictKind` moves from `asc-engine::merge` to `asc-core::conflict` and is re-exported, so the
engine type and the stored type are one. `CaseFold` is not stored: it is a land warning and not a
conflict in a tree (SPEC §4.4).

- **Encoding.** A new envelope kind: `b"asc" ‖ 7 ‖ 1 ‖ payload` in the canonical encoding of
  SPEC §2.2 (kind 7, version 1). The decoder goes through a `RawConflictSet` with
  `#[serde(try_from)]`, as `Changeset` does with `RawChangeset`, and rejects: unsorted or duplicate
  paths, an empty set, `adds.len() != removes.len() + 1`, more than `MAX_CONFLICT_SIDES` adds, a
  remove equal to an add (not simplified), all adds equal (resolved), `Term::Leaf` with kind `Dir`,
  `Shown::Side` with an index out of range or pointing at `Term::Absent`, `marker_len` outside
  7..=32, and `from` on any kind other than `FileDir`.
- **Identity.** `ConflictSetId` is a new typed id in `crates/asc-core/src/id.rs`: BLAKE3 of the
  envelope bytes, like `TreeId` and `MetaId`. It is stored under the new object key
  `ObjKey::ConflictSet(ConflictSetId)` with prefix `x/` (`crates/asc-store/src/key.rs`) and travels
  as `OBJ_KIND_CONFLICT_SET = 10` in `ObjKey` (`proto/asc/v1/repo.proto:8`).
- **Link from the changeset.** A conflicted changeset carries `extra["asc.conflicts"]` = the 32
  raw bytes of its `ConflictSetId`. A changeset with no conflict has no such key. `extra` is part of
  the changeset envelope, so the `ChangesetId` covers the set and through it every term.
- **The root tree holds a plain file at each conflicted path.** For `Shown::Markers` it is a
  `File` or `Exec` entry whose bytes are the materialised text with markers; for
  `Shown::Side { index }` it is exactly `adds[index]`. This is the file that `MergeOutcome.root`
  holds today, so the root tree stays an ordinary v1 tree, `apply(parent, changes) == root` and
  `changes == leafdiff(parent, root)` (SPEC §2.6 rules 2 and 3) keep holding, and every
  tree consumer (export, the synthetic index, turbo hashing, path index, code intelligence) keeps
  working without knowing about conflicts.

### Which stored formats change

- Envelope kind 7 `ConflictSet`: change: new; version: 1
- `Tree` (kind 1): change: none; version: stays 1
- `Changeset` (kind 2): change: new allowed `extra` key `asc.conflicts` (SPEC §2.6 table); version:
  stays 1
- Object key `x/` and `OBJ_KIND_CONFLICT_SET = 10`: change: new; version: n/a
- `AscGitExtra` (git `asc` header, SPEC §2.2a): change: v2 adds `conflicts`; written only for a
  conflicted changeset; version: 1 stays frozen, 2 new
- Server SQL: change: `cs_graph.conflicted BOOLEAN NOT NULL DEFAULT false`,
  `change_versions.conflicted_paths INTEGER NOT NULL DEFAULT 0`; version: new migration in both
  dialects
- Derived values (kinds 16-63): change: none; version: unchanged
- `state.db`: change: none; version: unchanged

No existing version byte moves. A store holds no conflicted changeset before the feature ships,
so the SQL defaults are correct for every existing row.

### Materialisation of the marker text

Labels are fixed, so the bytes do not depend on ref names or on changeset ids that a rebase
changes. `marker_len` starts at 7 and grows (up to 32) until no line of any term begins with a run
of `<`, `|`, `=` or `>` of that length followed by a space or the end of the line. A marker line
can then never be confused with content.

- **Two sides** (`adds.len() == 2`, every term present and text, every term at most
  `MAX_RENDER_TERM` bytes): the diff3 rendering of the existing text merge
  (`crates/asc-engine/src/merge/text.rs`), labels `side 1`, `base`, `side 2`:

  ```
  <<<<<<< side 1
  ...adds[0] lines...
  ||||||| base
  ...removes[0] lines...
  =======
  ...adds[1] lines...
  >>>>>>> side 2
  ```

  Regions that merge cleanly are merged, as git does. A plain git user sees git's diff3 shape.
- **More sides:** one block covering the lines from the first line where any two terms differ to
  the last such line; the common prefix and suffix stay outside. Sections in order:
  `<<<<<<< side 1`, `||||||| base 1`, `======= side 2`, `||||||| base 2`, ... ,
  `======= side n`, `>>>>>>> end`. The renderer is in `asc-core` (no diff needed).
- **Anything else** (a binary term, an absent term, a symlink or gitlink term, a term over
  `MAX_RENDER_TERM`, terms whose `Kind` differs between `File` and `Symlink`): `Shown::Side` with
  the first present add. This matches what git leaves in the work tree for binary and
  modify/delete conflicts. When adds differ only in `File`/`Exec`, the text is rendered and the
  entry takes the kind of `adds[0]`.

The materialised bytes are presentation, not truth. The terms are the truth. Ingest checks that a
`Markers` file parses under its `marker_len` with exactly `adds.len()` sides per block (a grammar
check, independent of the diff algorithm), and that a `Side` entry equals its term. It does not
re-render, so an upgrade of the text merge never invalidates a stored conflict. As for a clean
merge today, the bytes depend on the producer's merge library, so two producers agree when they run
the same release.

### Validation (new rule 7 of SPEC §2.6)

On every ingest (local commit, push, import, server mutation), for a changeset with
`asc.conflicts`:

1. The value is 32 bytes, the `ConflictSet` exists in the closure or the store, and it decodes.
2. Every term `Leaf` names a file that exists, and its `size` and `git` match (`file_meta` on the
   server, §8.4), exactly as for `Change::Write`.
3. The root entry at each conflicted path agrees with `shown` (above). For `FileDir`, `from`
   exists in the root as a directory.
4. **Inheritance.** Let `U` be the conflicted paths of `parents[0]` and of this changeset that
   `changes` does not touch (a path is touched when it, an ancestor or a descendant appears in
   `changes`). The entries of the two sets restricted to `U` are equal. A child therefore carries
   every conflict it did not edit, and cannot drop or invent one silently. Merge changesets are
   checked against `parents[0]` only (rule 6).
5. An empty set is never stored: a changeset whose conflicts are all resolved has no key.

Rule 4 has one relaxation: under `PathPolicy::Import`, a git commit without an `asc` header whose
parent is conflicted is accepted with no set. It is a plain git user's commit that took the marker
text as content (see Migration).

## Semantics

### Making a conflicted change

- `asc restack`, `asc sync`, `asc absorb`, `asc move` and every stack verb
  (`crates/asc/src/stack/cli/`): Record by default (`asc.restack.conflicts = record`; `stop`
  restores the pause).
- Server `Stacks.Restack`, `Changes.RestackChanges`, `Repo.Rebase`: Record only with the new
  `record_conflicts = true`. The forge-neutral default stays `false`, so existing callers see no
  change. MCP sets it to true.
- `asc commit --allow-conflicts`, `asc continue --allow-conflicts`: Record the unmerged stages 1/2/3
  of an in-progress operation as `removes = [stage 1]`, `adds = [stage 2, stage 3]`. Without the
  flag, git's refusal stands.
- A commit on top of a conflicted HEAD: Inherits the conflicts of paths it does not touch (rule 4);
  edited conflicted paths are read back (below).
- Land, `Repo.Merge`, `Repo.Apply`, git-surface `rebase`, `merge`, `cherry-pick`, `revert`: Never
  record.

The engine gets a policy instead of a new code path:

```rust
pub enum ConflictPolicy { Stop, Record }   // crates/asc-engine/src/merge/mod.rs
```

`rebase_one` takes it. With `Record`, a conflicted outcome stores the rewritten changeset with its
set and returns `Rebased::Done { id, merged: true, conflicted: true }`. `Rebased::Conflicts` stays
for `Stop` and for the cases that cannot be recorded (more than `MAX_CONFLICT_SIDES` sides, a set
over `MAX_CONFLICT_ENTRIES`). `Rewrite` (`crates/asc-engine/src/stack/restack.rs:102`) gains
`conflicted: bool`. `execute_restack` with `Record` returns `Outcome::Complete` unless a limit is
hit or the signer fails.

### Restack propagation

The tree merge (`crates/asc-engine/src/merge/tree.rs:130`) reads the conflict sets of its three
inputs. For a path where no input is conflicted, nothing changes. For a path where any input is
conflicted, the new module `crates/asc-engine/src/merge/conflicted.rs` computes
`onto - parent + change` over terms, flattens and simplifies, then:

1. one add left, or all adds equal: the path is resolved and written as an ordinary entry;
2. two adds, all text: the text merge runs on the three terms; a clean result resolves the path;
3. otherwise the path is recorded and materialised.

This is what makes a resolution in a parent flow into its children. Take a stack `C1 <- C2` where
both are conflicted at `f` and `C2` did not edit `f`: `C2.f` equals `C1.f`. Resolving `C1` to `x`
makes `C1'`. Restacking `C2` onto `C1'` gives `x - C1.f + C2.f = x`, so `C2'` is clean at `f` with
no further action. If `C2` did edit `f` (its value is a different conflict), the sum keeps only the
terms that `C2` changed.

The metadata-only rebase (`rebase_metadata`, `crates/asc-engine/src/rebase.rs:104`) is kept for
disjoint paths. It rebuilds the set from rule 4: entries for paths the change touches stay; entries
for the paths it does not touch come from the new parent's set. A metadata rebase therefore never
reads content, as today.

### Checkout and read-back

Materialisation costs nothing: the root tree already holds the marker text.

- **Plain work trees** (`crates/asc-wc/src/plain.rs`) write the tree as they do now. Checkout
  conversion of line endings (SPEC §2.4.1) applies to the marker file like any text file.
- **Virtual work trees** (SPEC §10, `crates/asc-vfs`) project the committed bytes. Sizes come from
  tree metadata and match, so no placeholder logic changes.
- **Staging** holds no stages for recorded conflicts. Stages 1/2/3 stay reserved for in-progress
  git operations, so `git commit` on top of a conflicted HEAD is not refused for "unmerged files".
- **Read-back.** A conflicted path whose work-tree file hashes to the root entry's id is unchanged
  and stays conflicted. A changed file is normalised as ingest normalises it (SPEC §2.4.1), then
  parsed with the recorded `marker_len` by the parser in `asc-core`:
  - no marker line of that length: resolved, and the content is the file;
  - a well-formed block structure: a new conflict whose terms are rebuilt from the file (text
    outside the blocks goes into every term, each section into its term), then simplified; one add
    left means resolved;
  - marker lines of that length that do not form a valid structure: `asc commit` and `asc add`
    refuse the path with
    `error: asc: <path> has broken conflict markers (fix them, or run 'asc resolve <path> --mark')`
    and exit 1.

  `asc add` of a recorded conflict differs from git only in the second case: a file that still
  parses as a conflict stays conflicted, and `asc add` prints
  `warning: asc: <path> still has conflicts; it stays conflicted`. Paths with git stages keep git's
  `add` behaviour.

### Diff

The root tree is ordinary content, so `asc diff`, `Repo.Diff` and `diffstat.v1` show the marker
text as the change it is: a conflicted change shows markers being added, and its resolution shows
them being removed. The per-file record of the API diff gains `conflicted: bool` (true when the
path is in the `to` side's set). `asc diff` prints one extra header line for such a file,
`asc: conflicted (N sides)`, only when the output goes to a terminal. Patch output for scripts and
`format-patch` stay byte-identical to git.

### Blame

The blame algorithm (`crates/asc-engine/src/blame.rs`) does not change, and `blame.v1` keeps its
version. Marker lines and base sections are attributed to the conflicted changeset, and they
disappear with the resolution. Side lines that came from the new parent continue into its history,
because the conflicted changeset's first parent holds them. Side lines from the rebased change are
attributed to that change's rewritten version, which is their author. `asc blame` of a path that is
conflicted at the blamed revision prints `warning: asc: <path> is conflicted at <rev>; marker lines
are attributed to it` on stderr.

### Status and log

- `asc status` (`crates/asc/src/cli/status.rs`) prints a section after the usual ones:

  ```
  Conflicts recorded in HEAD:
    (use "asc resolve <path>..." to resolve them)
  	conflicted:  src/lib.rs (2 sides, edited)
  	conflicted:  assets/logo.png (2 sides, binary)
  ```

  Porcelain v1 is unchanged. Porcelain v2 adds one header line per path,
  `# asc.conflict <sides> <path>`, which git's v2 readers skip as an unknown header.
  `WsStatusResponse` gains `repeated Conflict head_conflicts = 9`.
- `asc sl` and every stack view mark a conflicted change with `conflicted (N paths)` after the
  subject, in the conflict colour. git-surface `asc log` output is unchanged. It gains the format
  placeholder `%(asc:conflicts)`, which expands to the number of conflicted paths, or to nothing.
  `LogEntry` gains `uint32 conflicted_paths = 13`.

## The landing gate

A conflicted changeset can never become reachable from a shared ref: `master`, `release/*`,
`green/*`, the repository's land branches and tags (SPEC §2.7). That is stricter than "not the
tip". A clean change whose ancestor is conflicted cannot land either, because the ancestor would
enter trunk's history. The only exception is the `squash` land strategy when the squashed result
is clean, because the conflicted changesets never reach the ref.

The server enforces this in four places, in pipeline order (SPEC §8.3):

1. **Ingest** (`crates/asc-serve/src/ingest.rs`, `check_extra` at line 954 and the closure walk of
   step 4) validates rule 7, writes `cs_graph.conflicted`, and scans every term blob for secrets
   as if it were a written file.
2. **Land planning** (`crates/asc-serve/src/land.rs`, `plan` at line 261) refuses a stack that
   contains a conflicted changeset before any merge and before the fast-forward exits. The answer is
   the new error code `CHANGE_CONFLICTED` (gRPC `FailedPrecondition`) with `wire::Conflict` records
   read from the stored sets (`recorded = true`), at most `MAX_CONFLICT_PATHS`.
   `content_merge_land` always runs with `ConflictPolicy::Stop`.
3. **The ref gate** (`crates/asc-serve/src/policy/gate.rs`, `ServerGate::check` at line 1407) is
   the backstop for every update: plain pushes, `FAST_FORWARD`, `Repo.Merge`, `Repo.Commit` with a
   ref, `git push` through `asc-git`, and imports. For each `GateUpdate` whose ref is shared it runs
   one indexed query over the new closure,
   `SELECT ... FROM cs_graph WHERE id IN (<new ids>) AND conflicted`, and refuses with
   `CHANGE_CONFLICTED`. The closure is already ingested at this point, so this costs one query per
   update.
4. **The merge queue** (`crates/asc-serve/src/queue/mod.rs`) refuses to enqueue a conflicted
   change, and `Review` mergeability (`crates/asc-serve/src/changes/mergeable.rs`) answers
   `Conflicts` with the recorded records and runs no merge.

User refs, agent refs (`refs/heads/agent/*`), `refs/pull/<n>/head` and snapshots accept conflicted
changesets. These are exactly the refs that already accept unreviewed work.

In git-remote mode (no asc server), `asc push` applies the same rule on the client: it refuses to
push a conflicted changeset, or a descendant of one, to the remote's default branch or `release/*`,
and says why. The remote is a git host that cannot check.

**CI.** A CI run triggered by a conflicted change version reports the neutral result
`skipped: conflicted` and does not run its jobs. Required checks therefore stay unsatisfied, which
is a second reason the change cannot land. `asc ci checkout` (`crates/asc-ci/src/checkout.rs`)
refuses a conflicted revision with exit 1 unless `--allow-conflicts` is given. `Affected` works on
conflicted revisions (it reads metadata only) and counts each conflicted path as changed.

**Dependency gate.** The gate (`crates/asc-serve/src/deps/gate.rs`) does not parse a manifest or
lockfile at a conflicted path, because the marker text is not a manifest. It reports
`conflicted manifest <path>` as an informational finding on drafts. Shared refs never see such a
file, because step 3 refuses first.

## Protocol and CLI

### RPC changes (all additive)

- `ObjKind` (repo.proto:8): `OBJ_KIND_CONFLICT_SET = 10`
- `Conflict` (repo.proto:168): `uint32 sides = 8; repeated ConflictTerm terms = 9; bool recorded =
  10; ConflictShown shown = 11;`
- new `ConflictTerm`: `bool remove = 1; bool absent = 2; uint32 mode = 3; bytes file_id = 4; bytes
  git = 5; uint64 size = 6;`
- `RebaseRequest`: `bool record_conflicts = 11;`; `RebaseResponse` gains `repeated bytes conflicted
  = 4` (new ids that hold conflicts)
- `RestackStackRequest` (stacks.proto:44): `bool record_conflicts = 8;`; `RestackItemState` gains
  `RESTACK_ITEM_STATE_CONFLICTED = 7` (rewritten, conflicts recorded in `conflicts`)
- `RestackChangesRequest` (changes.proto:41): `bool record_conflicts = 9;`; `RestackChangesResponse`
  gains `repeated bytes conflicted = 4`
- `ChangeInfo` (changes.proto:21): `uint32 conflicted_paths = 12;`
- `LogEntry` (repo.proto:102): `uint32 conflicted_paths = 13;`
- `WsStatusResponse` (workspace.proto:47): `repeated Conflict head_conflicts = 9;`
- new `Repo.ListConflicts`: `(repo, rev, paths, limit, page_token) -> (repeated Conflict,
  next_page_token)`; repo read; path rules filter each entry
- new `Repo.Resolve`: `(repo, changeset, repeated Resolution, restack_descendants, update_ref,
  expect) -> (result, rewritten, remaining Conflict)`; repo write; mutating, `x-asc-op-id`
- `Resolution.op`: reuses `FileOp`; a new `take` oneof arm (`int32 side`, negative for a remove)
  picks a term
- `Repo.Version.features`: `conflicts.v1` when the server accepts conflicted changesets
- error codes (`crates/asc-rpc/src/errors.rs`): `CHANGE_CONFLICTED` with `ErrorDetail.conflicts`

`Repo.Resolve` amends the conflicted changeset rather than adding a child, keeps its change id
(next version) and, with `restack_descendants`, restacks the stack above it with
`record_conflicts`. An agent with no checkout resolves this way.

### CLI

- `asc resolve` (new, `crates/asc/src/cli/resolve.rs`):
  - `asc resolve [-r <rev>]` lists the conflicted paths of `<rev>` (default HEAD) with their
    sides, kinds and whether the work-tree file is edited;
  - `asc resolve <path>... --take <side>` takes one term (`1`..`n` for adds, `base` or `base<k>` for
    removes);
  - `asc resolve <path>... --mark` takes the work-tree content as it is, markers included;
  - `asc resolve <path> --tool` runs the configured `merge.tool` on a two-sided conflict;
  - `asc resolve` with paths and no option reads the edited files back (above).

  Each run amends the target change and restacks its descendants with `record`. It prints
  `resolved <a>, <b> left; restacked <c> descendants (<d> still conflicted)`. `--json` uses the
  JSON profile (`json.profile.v1`).
- `asc status`, `asc sl` and `asc log` as in Semantics.
- `asc commit --allow-conflicts` and `asc continue --allow-conflicts` as in Semantics.
- `asc restack --stop-on-conflict` overrides `asc.restack.conflicts` for one run.
- `asc land` and `asc push --land` print the `CHANGE_CONFLICTED` paths and
  `hint: resolve them with 'asc resolve -r <change>'`.

### MCP tool surface (`crates/asc-mcp/src/catalogue.rs`)

- Read tools: `status`, `log`, `smartlog` and `change_info` return `conflicted_paths`. A new read
  tool `conflicts` (rev, paths) returns the records, the terms and two-sided hunks, bounded by the
  token budget like every result.
- Write tools (stdio write toolset): `restack` sends `record_conflicts = true` and returns the
  conflicted changes, so an agent's restack never stops. A new tool `resolve` (rev, path, one of
  `take`, `content`, `mark`) calls `Repo.Resolve`. It is not destructive (the old version stays in
  the change history), so it needs no elicitation.
- The `land` tool returns `CHANGE_CONFLICTED` as a structured result with the paths, not as a
  transport error.

## Hub

The hub computes what it shows from its own data first. It asks asc only for what asc alone knows:
the conflict content.

From hub data (no asc call):

- The change-version event that asc already sends (`crates/asc-serve/src/notify/mod.rs`) gains
  `conflicted_paths`. The hub stores it per version.
- From that one number the hub shows: a `conflicted` badge with the count on change rows, stack
  views and the review header; a `conflicted` filter in change lists; land and queue actions
  disabled with the reason "resolve conflicts first"; and the CI row `skipped: conflicted`, which
  is the run result it already receives.
- Stack views mark which changes became clean after a restack, by comparing the stored counts of
  the old and new versions.

From asc (only when needed):

- The conflicts panel of the review page calls `Repo.ListConflicts` for paths, kinds and sides,
  and `Review.GetConflicts` for hunks. Both run when the panel opens, never for lists.
- "Take side" and "upload resolution" call `Repo.Resolve`. Both are subject to the same
  authorisation and gate as a push to the change's ref.

## Security and limits

- `MAX_CONFLICT_SIDES`: value: 8 adds (7 removes); where: `asc-core::conflict`; more sides are not
  recorded, and the restack pauses as today
- `MAX_CONFLICT_ENTRIES`: value: 10,000 paths per set; where: decoder and recorder; above it the
  restack pauses
- `MAX_CONFLICT_SET_BYTES`: value: 4 MiB encoded; where: decoder, before any allocation that the
  length prefixes ask for
- `MAX_RENDER_TERM`: value: 8 MiB per term, 32 MiB for all terms of a path; where: renderer; above
  it `Shown::Side`
- `MAX_READBACK_BYTES`: value: 64 MiB; where: read-back; a larger edited conflicted file is resolved
  only with `--mark`
- `marker_len`: value: 7..=32; where: decoder and parser

- **Untrusted input.** A `ConflictSet` from the wire is decoded through `RawConflictSet` with every
  check above, and each length prefix is compared with the remaining input before allocation. The
  marker parser is a single forward pass over lines: no regular expressions, no recursion, no
  nesting, and a marker is recognised only as exactly `marker_len` marker characters at the start
  of a line followed by a space or the end of the line. Paths are `RepoPath`s and get the same
  checks as changed paths (SPEC §2.8); a conflicted path is new to the changeset for
  `windows_valid` and case-fold checks.
- **Reachability.** Term files are referenced only from the set. Fetch want-lists
  (`crates/asc-engine/src/sync/`), `CopyClosure` and the garbage collector
  (`crates/asc-serve/src/gc.rs`) treat `x/` objects and their terms as reachable from the
  changeset, so a draft never loses a side.
- **Secrets and path rules.** Term files go through the push secret scan (ingest, step 1 of the
  gate). `Repo.ListConflicts`, `Review.GetConflicts` and the MCP `conflicts` tool filter terms by
  the caller's path rules (SPEC §15.5.1): a restricted path is listed as redacted with no ids.
  Agent scope checks apply to the resolved changeset like any other update.
- **Old clients meeting conflicted changesets.** An old client decodes the changeset (envelope v1)
  and sees a valid tree. Its validation refuses the unknown key `asc.conflicts`, so it cannot store
  or push the changeset, and it never requests `x/` objects. A child made by an old client on top of
  a checked-out conflicted change lacks the inherited entries, and a new server refuses it by rule 4
  with `INVALID_OBJECT` and the message `upgrade asc to commit on a conflicted change`. A new server
  leaves conflicted heads out of `cloud_heads` for a client whose `x-asc-client-version` predates
  the feature, and says so in the response message.
- **Old servers.** An old server refuses `asc.conflicts` at ingest (`ingest.rs:954`). A new client
  checks `Repo.Version.features` for `conflicts.v1` before it pushes a conflicted changeset, and
  refuses locally with `error: asc: the server does not accept conflicted changes; resolve them or
  upgrade the server`.

## Migration and compatibility

- **No history rewrite.** No object changes bytes or id. Changesets without conflicts are encoded
  exactly as today, and `AscGitExtra` v1 is still written for them, so their git ids are unchanged.
- **Rollout.** Servers first. The SQL migration adds the two columns with defaults. Then clients.
  The switch `[conflicts] enabled` (default true) and the repository setting
  `repos.conflicts` (default true) let an operator refuse conflicted changesets at ingest, which
  makes the server behave as an old one, with the same client message.
- **Specification.** SPEC §0 moves first-class conflicts from "Out of scope" into the design.
  §2.2 lists kind 7, §2.6 adds rule 7 and the `asc.conflicts` key, §2.2a and §2.9 describe
  `AscGitExtra` v2, §3.1 adds `x/`, §4.4 describes `ConflictPolicy`, §8.3 to §8.5 the gate, and
  §8.11 `Repo.Resolve`. Each unit below changes the sections it implements.

### Git export and import of a conflicted change

A conflicted change reaches git only through `asc export` or a git-remote push of a draft. The git
view never shows private refs, and shared refs cannot hold one. The export follows jj's precedent:

- The exported git tree is the root tree plus one top-level directory `.asc-conflicts/`. It holds,
  per entry in path order (`<i>` is the decimal index), `<i>/add-<k>` and `<i>/remove-<k>` blobs (or
  symlinks and gitlinks with their own modes) for every present term, and a `README` that explains
  the directory in two lines. A plain git user therefore checks out marker files plus the sides.
- The commit carries the `asc` header with `AscGitExtra` v2:

  ```rust
  pub struct AscGitExtra { pub copies: SortedMap<RepoPath, RepoPath>, pub snapshot: bool,
                           pub conflicts: Vec<GitConflict> }            // version 2
  pub struct GitConflict { pub path: RepoPath, pub kind: ConflictKind, pub adds: Vec<bool>,
                           pub removes: Vec<bool>, pub shown: Shown, pub from: Option<RepoPath> }
  ```

  The bools say which terms are present; the blobs come from `.asc-conflicts/<i>/`.
- `gitcommit(cs)` (SPEC §2.9) of a conflicted changeset renders that augmented tree.
- **Import.** `canonicalise_commit` (`crates/asc-core/src/git.rs:434`) accepts header versions 1
  and 2. With v2 it requires `.asc-conflicts/` to match the header exactly, strips it from the root,
  rebuilds the `ConflictSet` (term ids are recomputed from the blobs), and keeps the round-trip
  check. An export re-imported therefore gives the same `ChangesetId`. An unknown header version
  fails with `upgrade asc`, as SPEC §2.2a requires. Today the parser at `git.rs:408-413` says
  `bad asc header`, and unit 3 changes that message.
- A git commit with no `asc` header that contains `.asc-conflicts/` is ordinary content. A plain
  git user's commit on top of an exported conflicted commit imports as a clean child that took the
  marker text and the directory as content (the rule 4 relaxation). Its conflicted parent still
  keeps it off shared refs.

## Implementation units

Each unit compiles and passes its tests alone, in this order. Each adds its tests and updates the
SPEC sections and docs it implements (CLI.md, mcp.md, api.md where it touches them).

1. **Conflict model.** `crates/asc-core/src/conflict.rs` (new: `ConflictSet`, `Conflicted`,
   `Term`, `Shown`, `ConflictKind`, limits, the term algebra with flatten and simplify),
   `crates/asc-core/src/id.rs` (`ConflictSetId`), `crates/asc-core/src/codec.rs`
   (`EnvelopeKind::ConflictSet = 7`), `crates/asc-core/src/object.rs` (`EXTRA_CONFLICTS`,
   `Changeset::conflicts()`), `crates/asc-core/src/lib.rs`. Golden encodings and decoder
   refusals. SPEC §2.2, §2.6. Acceptance: `cargo test -q -j 4 -p asc-core conflict`.
2. **Markers.** `crates/asc-core/src/conflict/markers.rs` (new): marker-length choice, the N-side
   renderer, and the parser with term rebuild. Property test: render then parse gives a merge that
   is equal after simplification. Acceptance: `cargo test -q -j 4 -p asc-core markers`.
3. **Git header v2.** `crates/asc-core/src/git.rs` (export of the augmented tree, import with
   strip and rebuild, `upgrade asc` for unknown versions), `crates/asc-core/src/object.rs`
   (`AscGitExtra` decode of versions 1 and 2). Round-trip tests that keep the id. SPEC §2.2a,
   §2.9. Acceptance: `cargo test -q -j 4 -p asc-core git::conflict`.
4. **Object key.** `crates/asc-store/src/key.rs` (`ObjKey::ConflictSet`, prefix `x`), with
   the store backends' key round-trip tests. SPEC §3.1. Acceptance:
   `cargo test -q -j 4 -p asc-store key`.
5. **Validation.** `crates/asc-engine/src/commit.rs` (`validate_extra` accepts
   `asc.conflicts`; `validate_changeset` rule 7 with the import relaxation). Acceptance:
   `cargo test -q -j 4 -p asc-engine commit::conflicts`.
6. **Conflicted merge.** `crates/asc-engine/src/merge/conflicted.rs` (new),
   `crates/asc-engine/src/merge/tree.rs` (read input sets, record),
   `crates/asc-engine/src/merge/mod.rs` (`ConflictPolicy`, `MergeOutcome.recorded`).
   Tests: two-sided, three-sided, cancellation, binary,
   modify/delete, file/directory. SPEC §4.4. Acceptance:
   `cargo test -q -j 4 -p asc-engine merge::conflicted`.
7. **Rebase and restack.** `crates/asc-engine/src/rebase.rs` (policy, metadata rebase rebuilds the
   set), `crates/asc-engine/src/stack/restack.rs` (`Rewrite.conflicted`, no pause under `Record`),
   `crates/asc-engine/src/land.rs` (`Stop` always; refuse conflicted input). Tests include "a
   resolution in the parent cleans the child". Acceptance:
   `cargo test -q -j 4 -p asc-engine restack`.
8. **Resolve and sync.** `crates/asc-engine/src/merge/resolve.rs` (new: amend with take, content or
   mark, then restack descendants), `crates/asc-engine/src/sync/` (want-lists include `x/` and
   terms). Acceptance: `cargo test -q -j 4 -p asc-engine resolve`.
9. **Protocol.** `proto/asc/v1/repo.proto`, `stacks.proto`, `changes.proto`, `workspace.proto`,
   `crates/asc-rpc/src/errors.rs` (`CHANGE_CONFLICTED`), the conversions in `crates/asc-rpc/src/`.
   Acceptance: `cargo test -q -j 4 -p asc-rpc conflict`.
10. **Server ingest.** `crates/asc-serve/src/ingest.rs` (rule 7, term `file_meta` checks, term
    secret scan, `cs_graph.conflicted`), `crates/asc-serve/src/gc.rs` (reachability),
    `crates/asc-serve/migrations/{postgres,sqlite}` (both columns). SPEC §8.2, §8.4. Acceptance:
    `cargo test -q -j 4 -p asc-serve ingest_conflict`.
11. **Landing gate.** `crates/asc-serve/src/policy/gate.rs`, `crates/asc-serve/src/land.rs`
    (`plan` refusal before the fast-forward exits), `crates/asc-serve/src/queue/mod.rs`,
    `crates/asc-serve/src/changes/mergeable.rs`. Tests cover fast-forward push, land, `Repo.Merge`,
    `git push`, the squash exception and a clean child of a conflicted parent. SPEC §8.3, §8.5.
    Acceptance: `cargo test -q -j 4 -p asc-serve conflicted_gate`.
12. **Server RPCs.** `crates/asc-serve/src/rebase.rs`, `crates/asc-serve/src/changes/restack.rs`
    (`record_conflicts`), `crates/asc-serve/src/conflicts_rpc.rs` (new: `ListConflicts`,
    `Resolve`), the `conflicts.v1` feature, the `cloud_heads` filter for old clients. SPEC §7,
    §8.11. Acceptance: `cargo test -q -j 4 -p asc-serve conflicts_rpc`.
13. **CI, dependencies and events.** `crates/asc-serve/src/ci/mod.rs` (`skipped: conflicted`),
    `crates/asc-serve/src/deps/gate.rs`, `crates/asc-serve/src/notify/mod.rs`
    (`conflicted_paths`), `change_versions.conflicted_paths` writes. Acceptance:
    `cargo test -q -j 4 -p asc-serve conflicted_ci`.
14. **Work-tree read-back.** `crates/asc-wc/src/conflict.rs` (new: read-back over plain and
    virtual files), `crates/asc-wc/src/status.rs`, `crates/asc-wc/src/vstatus.rs`. SPEC §9.5,
    §10. Acceptance: `cargo test -q -j 4 -p asc-wc conflict`.
15. **CI checkout.** `crates/asc-ci/src/checkout.rs` (refusal and `--allow-conflicts`).
    Acceptance: `cargo test -q -j 4 -p asc-ci checkout`.
16. **CLI.** `crates/asc/src/cli/resolve.rs` (new), `crates/asc/src/cli/status.rs`,
    `crates/asc/src/cli/commit.rs`, `crates/asc/src/cli/add.rs`,
    `crates/asc/src/stack/cli/restack.rs`, `crates/asc/src/stack/cli/continue_.rs`,
    `crates/asc/src/stack/cli/sl.rs`, `crates/asc/src/cli/log.rs`, `crates/asc/src/cli/push.rs`
    (git-remote client gate). docs/CLI.md. Acceptance: `cargo test -q -j 4 -p asc resolve`.
17. **MCP.** `crates/asc-mcp/src/catalogue.rs`, `crates/asc-mcp/src/handler/` (`conflicts`,
    `resolve`, `restack` default). docs/mcp.md. Acceptance:
    `cargo test -q -j 4 -p asc-mcp conflicts`.

## Open questions

1. **Default for local stack verbs.** Record or pause? Recommendation: record
   (`asc.restack.conflicts = record`), as jj does. A person sees the conflicted changes listed at
   the end of the run, and `--stop-on-conflict` gives back the pause.
2. **Should git-surface `rebase` and `cherry-pick` record?** Recommendation: no. They stay git's.
   Recording through them is explicit, with `--allow-conflicts` on `commit` or `continue`.
3. **Porcelain status.** Should committed conflicts appear as `UU`? Recommendation: no. Porcelain
   v1 is unchanged, and v2 gets the `# asc.conflict` header, because `UU` implies an operation in
   progress and stages that do not exist.
4. **A stack with a conflicted interior change and a resolution on top.** Recommendation: refuse
   the rebase land, and allow only the `squash` strategy when its result is clean. `asc resolve -r`
   amends the right change, so the normal flow never builds such a stack.
5. **File/directory conflicts.** Record them or keep pausing? Recommendation: record them with the
   ort-style move to `<path>~side 2` and `from`. Resolution is an ordinary edit followed by
   `asc resolve --mark`. Case-fold pairs stay a land warning.
6. **Hub editing.** Should the hub have an inline editor in the first release? Recommendation: no.
   Ship take-side and upload; the editor is additive later and needs no format change.
7. **Side limit.** Is 8 enough? Recommendation: yes. More than 8 sides means the stack is being
   restacked over unrelated churn, and a pause is the right signal. The limit is a decoder
   constant, so raising it later is a compatible change for readers that know the higher limit.
8. **Renderer for more than two sides.** Hunk-level or one region? Recommendation: one region in
   the first release. Ingest never re-renders, so a better renderer later needs no format change.
9. **Git export.** Export the `.asc-conflicts/` directory, or refuse to export conflicted changes?
   Recommendation: export, following jj. Refusing would block git-remote mode for drafts, which is
   the case this feature exists for.
10. **Old clients and cloud heads.** Hide conflicted heads from old clients, or let them fail on
    validation? Recommendation: hide them, with a message, so an old client's `asc sl` keeps
    working during a rolling upgrade.
