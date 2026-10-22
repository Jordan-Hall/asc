# Branchless by default

Status: design for the checklist decision "branchless by default" (2026-10-22, `scion-checklist.md`). It is not normative until it is folded into SPEC.md and PLAN.md; §8 lists the edits those files need. It builds on `stacks.md` (SPEC §20): `ChangeMeta`, the `changes` index, `change_versions`, the `Changes` service and D1–D12 are used here and not restated. In PLAN the stacks.md S-units are numbered C-units (S24 = C24); this file uses the PLAN ids.

Inputs: the Scion article (refs discouraged except main and release; changesets server-side and visible at once; refs are a single-row CAS separate from changeset creation), Sapling and Commit Cloud (`sync.py`, `workspace.py`, `hide`, smartlog), jj (visible heads, anonymous branches, `refs/jj/keep`, 2-week GC), git-branchless (keepalive refs, `submit`), Gerrit (`refs/for`, `refs/changes`), Mercurial infinitepush, Piper/CitC, git `reftable` and `hideRefs` (research of 2026-10-22).

Fixed rules this design obeys: git commands keep git's exact semantics, output and exit codes; the default branch is `master`; there is no IDE git plumbing; there is no LFS; memory is bounded and nothing scans the whole repo; local-only mode keeps working.

---

## 0. Decisions

| # | Decision | Why |
|---|---|---|
| BD1 | **A draft is a non-immutable changeset that has a `ChangeMeta`** (stacks §2.2). Nothing names it except its change id and changeset id. A branch is never needed. | Scion and Sapling: the draft/public boundary decides what is work in progress, not a ref. |
| BD2 | **Visibility is a set of draft heads, not refs.** Local: `state.db.draft_heads`. Server: `cloud_heads` per user cloud workspace. Visible drafts are the draft ancestors of visible heads. | jj's view and Commit Cloud's heads. Unlike git-branchless, no keepalive refs appear in `for-each-ref`, and advertisement cost does not grow with drafts (the infinitepush lesson). |
| BD3 | **A local branch is a real local ref** (`state.db.refs`, `refs/heads/<b>`), exactly as today. In branchless mode it is a *label*: it is never a shared server ref. | Every local git command then behaves and prints exactly as git, in both modes (`On branch X`, `HEAD detached at`, `for-each-ref`, turbo's `HEAD`). |
| BD4 | **Shared refs are only the patterns in the repo's `shared_refs`** (default `refs/heads/master`, `refs/heads/release/*`, `refs/heads/green/*`, `refs/tags/*`). Every other pushed `refs/heads/<n>` becomes a **user ref** `refs/users/<owner-id>/heads/<n>`: one CAS row in the existing `refs` table, advertised only to its owner (as `refs/heads/<n>`). | Scion principle 6 holds: no global state per draft. The checklist's "user-scoped branches". Git push/fetch semantics stay intact for the owner. |
| BD5 | **`git push origin feat` in branchless mode = user-ref CAS plus submit of the stack** in one `UpdateRefs`: `refs/heads/feat` (user ref) plus one `refs/for/<trunk>/asc-<change>` per change of `trunk..feat` (stacks D8, C24). PRs are created as **drafts** (`asc.push.review`). Output is git's push output for `feat`. | The checklist line "pushing a branch submits its stack as draft changes". Rejections, `-u`, `--force-with-lease` and `--delete` keep git's semantics because the user ref is a real CAS row. |
| BD6 | **Commit cloud:** when online with a daemon, each new changeset and its `ChangeMeta` are uploaded asynchronously (`HasObjects`, `PutObjects`, `Ingest`), then the head set is updated with a single-row CAS (`Cloud.UpdateWorkspace`). Nothing waits for it. Upload is want-only (explicit ids, no have/want negotiation, no packs) and idempotent. | Scion: "nothing to push". `push`/`submit` then find nothing to upload. Local-only and offline work are unchanged. |
| BD7 | **No git command changes its local behaviour or output with the mode.** The compat suite runs in both modes and must produce byte-identical results. Mode differences are confined to: what the server stores for a non-shared push, whether drafts upload in the background, and `asc`-only verbs. | stacks D4 and the git-identical rule. |
| BD8 | **Hidden is never deleted at once.** `asc hide` (and rewrites, `asc abandon`, cloud removal) drop heads; objects stay addressable by id. Server: hidden heads stay GC roots for 30 days; visible heads untouched for 180 days with no open PR are auto-hidden. Local: hidden drafts follow the op-log bounds (90 days). | Sapling hide/unhide, jj's 2-week GC, Commit Cloud's age-based omission. Recovery is always `asc unhide <id>` within the grace period. |
| BD9 | **Scope:** drafts are owned by the authenticated Forgejo user. Objects are readable by id by anyone with repo read (the namespace rule, SPEC §15.1, unchanged). Listing another user's cloud workspace needs repo write. Only the owner (or an admin, for hide/delete) changes a workspace or a user ref. | Scion's "available everywhere" for content, while the listing of work in progress stays among collaborators. |
| BD10 | **Two switches.** Client `asc.branchless` (bool, default `true`; system/global/local/worktree/`-c`/env) for client behaviour, and the server repo policy `repos.branchless` (default `true` for new repos and after migration) for ref policy. Per-user opt-out is the client key; per-repo opt-out is the server setting. | The checklist: "switch it off per repo or per user". The server decides what a ref is; a client cannot create shared refs by configuration. |
| BD11 | **Actions and webhooks fire only for shared refs and PRs.** User refs emit no Forgejo push event and no branch row. CI for drafts runs on `pull_request` events of the per-change PRs. | No global fan-out per draft. ASC's workflows already use `pull_request` plus `push: [master]`. |
| BD12 | **Branchless server policy needs an asc-primary repo (Stage ≥ 2).** Stage 0 (git remote) and Stage 1 (mirror) are branch mode on the server; the client keeps local draft heads only. | Forgejo git is the truth before Stage 2, and it has branches. |

---

## 1. Model

### 1.1 Terms

| Term | Definition | Cost |
|---|---|---|
| draft | stacks §2.2 "draft change": not reachable from a trunk or `asc.stack.immutable` ref, has a `ChangeMeta` | `is_ancestor` via skip lists |
| draft head | A row in `draft_heads` (local) or `cloud_heads` (server) | O(1) |
| visible draft | A draft that is an ancestor of a visible head, found by a first-parent walk from the head that stops at the first immutable changeset (stacks C05) | O(heads × stack length × log n) |
| hidden | A head with `hidden_at` set. Its drafts are invisible unless another visible head reaches them. | O(1) |
| obsolete | stacks §2.2: a visible successor names it in `preds` | local index |
| cloud workspace | `(repo, owner, name)`, default name `default`; optional per-host name | one row |
| user ref | `refs/users/<owner-id>/heads/<n>` in the server `refs` table | one row, CAS |

### 1.2 Local state (`state.db`, additive)

```sql
draft_heads(repo_id, cs BLOB, change BLOB, added INT, hidden_at INT NULL, PRIMARY KEY(repo_id, cs))
  + INDEX (repo_id, hidden_at)
cloud_queue(repo_id, cs BLOB, enq INT, attempts INT, next_at INT,
            state INT /*0 pending, 1 failed, 2 too_large*/, last_error TEXT, PRIMARY KEY(repo_id, cs))
cloud_state(repo_id PRIMARY KEY, ws_name TEXT, version INT, last_sync INT)
cloud_base(repo_id, cs BLOB, PRIMARY KEY(repo_id, cs))      -- head set at last successful sync (3-way base)
```

Maintenance runs in the same transaction as the ref and op-log write (stacks C04 sinks), so it costs a few rows per command:

| Event | `draft_heads` | `cloud_queue` |
|---|---|---|
| new changeset X (commit, merge, cherry-pick, `asc create`) | insert X; delete each parent of X that is a head | upsert X; delete queued parents not yet started (coalesce: uploading X uploads them) |
| rewrite old → new (amend, rebase, restack, absorb, …) | delete `old` if it is a head; insert `new` (with descendants' new ids) | as for a new changeset |
| commit becomes immutable (fetch moves a trunk over it, land) | delete heads that are now immutable | delete queued entries that are now immutable |
| `asc hide <rev>` | set `hidden_at` on every head that has `<rev>` as an ancestor; insert `<rev>`'s parent as a head when it is a draft with no other visible child | remove those heads from the queue |
| `asc unhide <id>` | clear `hidden_at`, or insert the id as a head | enqueue |

Maintained in both modes (it is invisible). Only branchless mode reads it for `asc sl`, upload and GC.

### 1.3 Server state (additive)

```sql
cloud_ws(ns BLOB, owner BIGINT, name TEXT, version BIGINT, updated BIGINT, PRIMARY KEY(ns, owner, name))
cloud_heads(ns BLOB, owner BIGINT, name TEXT, cs BLOB, change BLOB, labels TEXT /*json, display only*/,
            added BIGINT, hidden_at BIGINT NULL, PRIMARY KEY(ns, owner, name, cs))
  + INDEX cloud_heads_hidden (ns, hidden_at), INDEX cloud_heads_added (ns, added)
repos += branchless BOOL, shared_refs TEXT /*json globs*/, legacy_shared TEXT /*json names, migration only*/
-- user refs: rows in the existing refs/ref_log tables (SPEC §3.4), name = refs/users/<owner-id>/heads/<n>
```

The draft index by change id is the existing `changes` table and `changes_by_change` index (stacks §4.4): `Ingest` already writes a row for every changeset whose closure carries a valid `m/` object. `cloud_heads` adds only the visible set. Stack shape stays derived from the graph (stacks D3).

### 1.4 GC roots and expiry

| Root or rule | Local | Server |
|---|---|---|
| Visible heads | `draft_heads` with `hidden_at IS NULL`: their objects stay in `store/` until uploaded, then demote to `cache/` (as after push, SPEC §6.3 step 5) | every `cloud_heads.cs` (visible or hidden within grace) is a GC root (amends SPEC §3.6) |
| Hidden heads | kept while an op-log row references them (`asc.opLog.maxDays` 90); then `asc gc` deletes un-uploaded objects only they reach | rows with `hidden_at < now − hidden_grace_days` (30) are deleted by the expiry worker; the objects become collectable by O01's two-mark sweep |
| Stale visible heads | none (local work is never auto-hidden) | `added < now − abandon_days` (180) and no open PR for any change in the stack → `hidden_at = now` |
| Submitted versions | – | `change_versions` (stacks §4.4), unchanged |
| User refs | – | ordinary refs (roots); `agent/*` user refs keep their TTL (SPEC §8.8); a user ref whose changes have all landed is deleted after `landed_ref_days` (7, 0 disables) |
| Pull window | – | heads older than `asc.cloud.maxSyncAge` (14 days) are not pulled to new machines; `asc cloud list --all` shows them |
| Secret purge | – | `asc cloud purge <change>` (repo admin): deletes the heads and lists the objects in `gc_purge`; O01 sweeps a purged object at its next run if no ref or head reaches it, without the 14-day age rule |

The worker runs hourly in batches of 1,000 rows using the two indexes; it never scans `changes`.

### 1.5 Scoping and access

| Action | Owner | Repo write | Repo read | Repo admin |
|---|---|---|---|---|
| Read draft objects by id (`GetObjects`, `ResolveChanges`, `Stack`, `asc stack get <change>`) | yes | yes | yes | yes |
| List a cloud workspace (`Cloud.GetWorkspace`, `asc sl --cloud --user u`) | yes | yes | no | yes |
| Update a workspace (add, remove, hide) | yes | no | no | hide only |
| Push or delete a user ref | yes | no | no | delete only |
| Fetch another user's ref by explicit refspec `refs/users/<login>/heads/<n>` | yes | yes | no | yes |

---

## 2. Commit cloud

### 2.1 Upload pipeline (daemon)

1. Take the oldest `pending` row of `cloud_queue` whose `next_at ≤ now`.
2. Closure: the new changesets from the head back to the first one the server knows (the remote-tracking tips and the last synced heads bound the walk), their new trees by spine, files, chunks and `m/` objects. This is push step 1 (SPEC §6.3) with a 256-id batch window.
3. `HasObjects` in batches of 4,096, then `PutObjects` of the missing ids only, files and chunks first, trees bottom-up, changesets last. Objects are stored bytes; puts of content-addressed objects are idempotent.
4. `Ingest` the changesets (SPEC §8.7). It writes `cs_graph`, `git_map` and `changes` rows, with no ref.
5. Sync the head set (§2.4) with one `Cloud.UpdateWorkspace` CAS carrying an `op_id` (idempotent through `op_results`).
6. Delete the queue row; demote the uploaded objects from `store/` to `cache/`.

`push`, `submit` and `asc cloud sync` drain the queue entries in their closure first; because every step is idempotent, a race with the daemon costs only a repeated `HasObjects`.

### 2.2 Bounds

| Bound | Value | Behaviour at the bound |
|---|---|---|
| Queue length | ≤ visible heads (coalesced, §1.2) | `asc.cloud.queueMax` 10,000 heads; beyond it new heads stay local and `asc cloud status` reports `queue full` |
| Changeset size | `asc.cloud.maxChangesetSize` 1 GiB of new content | the entry is marked `too_large`; `push` still uploads it |
| Concurrency | `asc.cloud.streams` 2 `PutObjects` streams, ≤ 16 MiB batches | – |
| Daemon memory | the batch window only; closure walked spine by spine | within the 64 MiB idle and bounded-load budget (SPEC §16.4) |
| Server | `PutObjects` in class `objects`; `Ingest` and `UpdateWorkspace` in a new class `background` that is shed first | `RESOURCE_EXHAUSTED` with `retry-after-ms` |
| Server heads | `[cloud] max_heads_per_workspace` 10,000 | `CLOUD_HEADS_LIMIT`; the user hides heads |

### 2.3 Retry, offline and modes

| Situation | Behaviour |
|---|---|
| Transient error (`UNAVAILABLE`, `RESOURCE_EXHAUSTED`, `DEADLINE_EXCEEDED`) | backoff with full jitter from 1 s to 5 min, unbounded attempts, honouring `retry-after-ms` |
| Permanent error (`INVALID_OBJECT`, `PERMISSION_DENIED`, `PATH_POLICY`) | `state = failed`, `last_error` kept, retried only when the head changes or on `asc cloud sync` |
| Offline (connect fails) | the queue pauses; the daemon probes `Ping` with the same backoff; commands never block or print anything |
| No daemon (`ASC_NO_DAEMON`, CI jobs, tests) | nothing uploads in the background; `push`, `submit` and `asc cloud sync` upload in-process |
| local-only, bucket remote, git remote, mirror-mode server, or server without the `cloud.v1` feature | cloud is off; draft heads stay local |
| `asc.cloud=off` or `asc.branchless=false` | cloud is off |

### 2.4 Multi-machine sync (Commit Cloud algorithm)

1. Upload local queued heads (§2.1 steps 1–4).
2. `GetWorkspace(since = cloud_state.version, max_age = maxSyncAge)` returns the remote heads.
3. Three-way merge against `cloud_base`: union of heads; a head removed on one side since the base is removed on the other (locally it gets `hidden_at`, so it is recoverable); a remote head that has a local head in its `preds` closure replaces it.
4. `UpdateWorkspace(base_version, add, remove)`. On `VERSION_CONFLICT`, repeat from step 2, at most 3 times, then report it in `asc cloud status`; the next tick retries.
5. Pull the metadata of new remote heads (`Changes.Stack`, `LogEntry`s only, stopping at immutable changesets). Files stay lazy.

Rules: sync never moves `HEAD`, a branch or the working copy. A checked-out change that became obsolete is shown by `asc sl`, and the user moves with `asc next` or `asc sync`. Labels are synced as display text only and never become `refs/heads/*` on another machine (git's branches are local). The daemon syncs every 5 minutes, on `Watch` events of kind `KIND_CLOUD`, and when `git fetch` or `asc sl` runs.

### 2.5 RPCs (`proto/asc/v1/cloud.proto`, same port)

```proto
service Cloud {
  rpc GetWorkspace(GetWorkspaceRequest) returns (GetWorkspaceResponse);          // cheap
  rpc UpdateWorkspace(UpdateWorkspaceRequest) returns (UpdateWorkspaceResponse); // background, x-asc-op-id
  rpc ListWorkspaces(ListWorkspacesRequest) returns (ListWorkspacesResponse);    // cheap, paged
  rpc Purge(PurgeRequest) returns (PurgeResponse);                               // admin only
}
message CloudHead { bytes cs = 1; bytes change = 2; repeated string labels = 3; int64 added = 4; int64 hidden_at = 5; }
message GetWorkspaceRequest { RepoRef repo = 1; string owner = 2; /* empty = caller */ string name = 3;
  uint64 since_version = 4; int64 max_age_s = 5; bool include_hidden = 6; string page_token = 7; }
message GetWorkspaceResponse { uint64 version = 1; repeated CloudHead heads = 2; repeated bytes removed = 3;
  uint32 omitted = 4; string next_page_token = 5; }
message UpdateWorkspaceRequest { RepoRef repo = 1; string name = 2; uint64 base_version = 3;
  repeated CloudHead add = 4; repeated bytes remove = 5; repeated bytes hide = 6; repeated bytes unhide = 7; }
message UpdateWorkspaceResponse { uint64 version = 1; }   // ErrorDetail.code VERSION_CONFLICT carries the current version
// repo.proto, additive: Version.features += "cloud.v1", "branchless.v1"; ErrorDetail.code += VERSION_CONFLICT,
//   CLOUD_HEADS_LIMIT, REF_NOT_SHARED; RepoConfigResponse += branchless, shared_refs; WatchEvent kind += KIND_CLOUD.
```

### 2.6 Budgets (benchmark gate, O05)

| Metric | Target |
|---|---|
| `commit` latency with the daemon uploading | unchanged within noise (the enqueue is one row in the existing transaction) |
| Time from commit to visible on the server, ≤ 10 changed files, online | p50 < 2 s, p95 < 10 s |
| `push` of an already uploaded stack | one `HasObjects` batch plus one `UpdateRefs` |
| `UpdateWorkspace` | O(changed heads), one row CAS plus head rows |
| `ListRefs` for one user | O(shared refs + own user refs); independent of the number of drafts and users |

---

## 3. Git DX mapping

### 3.1 What a branch is

- **Local:** a real ref in `state.db.refs`, as today. `HEAD` in the synthetic gitdir is `ref: refs/heads/<b>` on a branch and a SHA when detached (SPEC §9.4). asc never invents or hides a branch.
- **Status line:** `On branch <b>` when `HEAD` is symbolic, `HEAD detached at <sha>` / `HEAD detached from <sha>` when detached, exactly as git. An anonymous draft is a detached `HEAD`.
- **Leaving commits behind:** `checkout`/`switch` away from detached drafts prints git's "Warning: you are leaving N commits behind…" byte for byte. The commits stay visible through `draft_heads`, so `asc sl` still shows them; the warning is not changed (BD7).
- **Server:** in a branchless repo a pushed non-shared branch is the pusher's user ref (BD4), so `origin/<b>` exists for its owner only.

### 3.2 Command table

"Same" means git's semantics, output and exit code, with the refs that exist.

| Command | Branchless mode | Branch mode |
|---|---|---|
| `branch`, `branch -d/-D/-m/-c/-u`, `branch -r/-a/-vv` | Same (local refs). `-r` lists shared refs plus the caller's own user refs. | Same; `-r` lists every server branch |
| `checkout -b`, `switch -c/-C`, `checkout <b>` (DWIM from `origin/<b>`) | Same. DWIM works for the owner's pushed branches. | Same |
| `checkout <sha>`, `switch --detach` | Same; commits made there become draft heads | Same; commits become draft heads (unused) |
| `status` | Same (`On branch` / `HEAD detached`, ahead/behind against `origin/<b>`). Cloud upload never changes ahead/behind. | Same |
| `commit`, `merge`, `rebase`, `cherry-pick`, `reset`, `stash` | Same; draft heads and the upload queue are updated invisibly | Same; draft heads only |
| `tag` (local) | Same | Same |
| `push origin feat` (non-shared name) | User-ref CAS plus submit of `trunk..feat` (§3.3) | Creates or updates the server branch (git) |
| `push origin master` / `release/x` / tags in `shared_refs` | Same: shared-ref CAS, branch protection, `--land`/`-o land` | Same |
| `push origin <tag>` not in `shared_refs` | `! [remote rejected] <tag> -> <tag> (asc: ref not shared in this repository)`, exit 1 | Same as git |
| `push --delete feat` | Deletes the user ref; closes the open PRs of changes that no other user ref or cloud head of the owner reaches | Deletes the server branch |
| `push --force`, `--force-with-lease` | CAS on the user ref with git's rules; the PRs get new versions (lease of stacks §4.2) | Same as git |
| `push -u`, `push` with `push.default` | Same (upstream `origin/feat`) | Same |
| `push --all` | Each local branch as above; `--mirror` acts on the caller's visible set only | Same as git |
| `fetch`, `fetch --prune` | Same over the advertised set (shared + own user refs + `refs/pull/*` if configured). Also runs a cloud pull, which prints nothing. | Same over all server branches |
| `pull` | Same (fetch, then merge or rebase per config) | Same |
| `ls-remote` | Same over the advertised set | Same |
| `log --branches`, `log --all`, `for-each-ref`, `show-ref` | Same: refs only. Anonymous drafts are not refs and do not appear (no `refs/branchless/*`-style keepalives) | Same |
| `rev-parse`, `rev-parse --abbrev-ref HEAD`, `symbolic-ref` | Same | Same |
| `worktree add -b agent/<n>` (Claude Code hooks) | Same; `push -o asc.ttl=7d` makes an expiring user ref | Same; expiring shared branch |
| `asc create` | No label unless a name is given; on a branch the branch advances as with `commit` | Names a branch from `asc.stack.branchTemplate` (C11) |
| `asc submit` | As C24; also updates the user ref of the current branch if it has one | As C24 |
| `asc sl` | Heads = local branches + workspace `HEAD`s + visible draft heads; `--cloud` adds other machines' heads | Heads per stacks §2.2 |

### 3.3 `git push origin feat` in branchless mode

1. Resolve the stack trunk of `feat` (C05). Collect `trunk..feat` first-parent. Commits without a `ChangeMeta` get a derived id (B20).
2. Upload the closure (§2.1; usually already done).
3. One `UpdateRefs` (`--atomic` semantics): `refs/heads/feat` (the server maps it to the user ref; expect = the `origin/feat` value or the lease) plus `refs/for/<trunk>/asc-<change>` per change, bottom to top, with push option `asc.draft` per `asc.push.review`. If the range contains a merge changeset, it sends one AGit ref for the tip change covering the whole range (one classic PR).
4. Any per-change rejection (for example `not allowed to update change <short>`) fails the whole update: `! [remote rejected] feat -> feat (<reason>)`, exit 1, nothing moved.
5. Output is git's for the `feat` line only, plus the server's `remote:` lines (PR URLs), which git also prints verbatim:

```
remote: asc: 3 changes: 2 new draft PRs, 1 updated
remote:   https://forge.example.org/asc/monorepo/pulls/412
To https://forge.example.org/asc/monorepo.git
 * [new branch]      feat -> feat
```

`asc.push.review = none` sends only the user ref; `publish` creates ready PRs. The compat suite strips `remote:` lines, as it already does for server-specific text.

### 3.4 Turbo and CI

| Caller | Effect |
|---|---|
| turbo (`.git/HEAD`, `.git/index`, `rev-parse`, `ls-files`, `diff-tree --merge-base`, `status`) | none: all local, branches are real refs |
| `TURBO_SCM_BASE`/`HEAD` in Actions | PR events give `base_ref = master` and the PR head; unchanged |
| `asc ci checkout` | checks out `refs/pull/<n>/head`, a changeset id or a shared ref; it never needs a user ref. CI has no daemon, so it uploads nothing |
| Workflows `on: pull_request` | fire for every per-change PR, drafts included |
| Workflows `on: push: branches: [master]`, `release/*`, `green/*` | unchanged |
| Workflows `on: push` for other branches | no longer fire in a branchless repo; `asc doctor --workflows` lists them (B24) |

---

## 4. Server

### 4.1 Ref policy (applied in `UpdateRefs` step 1 and in `ListRefs`)

| Ref pushed by a client | Branchless repo | Branch-mode repo |
|---|---|---|
| matches `shared_refs` | shared ref: CAS, pre-receive (branch protection), outbox `post_receive`, bridge | same |
| other `refs/heads/<n>` | user ref `refs/users/<owner-id>/heads/<n>`: CAS, owner-only, no pre-receive, no `post_receive`, no bridge, TTL allowed for `agent/*` | shared branch |
| other `refs/tags/*` | rejected `REF_NOT_SHARED` | shared |
| `refs/for/*` | AGit (SPEC §13.5, C34) | same |
| `refs/pull/*`, `refs/snapshots/*` | server-managed, as today | same |
| legacy git push through `ImportGitPush` to a non-shared branch | rejected with git's `! [remote rejected] <b> -> <b> (asc: branchless repository; push to refs/for/master)`; AGit pushes work | shared branch |

`shared_refs` defaults to `[refs/heads/<default_branch>, refs/heads/release/*, refs/heads/green/*, refs/tags/*]` plus `land_branches`; it is set per repo through `RepoAdmin` and the Forgejo repo settings (B17). `ListRefs` returns shared refs plus the caller's own user refs renamed to `refs/heads/<n>`; an explicit `refs/users/<login>/` prefix lists another user's (write permission). A user ref and a shared ref can never collide, because the name decides the class.

### 4.2 Forgejo

| Concern | Branchless behaviour |
|---|---|
| PRs | One AGit PR per change, `base_branch` = trunk, `merge_base` = parent commit (stacks D8, C34). No head branch exists. Push creates them as drafts by default. |
| Branch-chain PRs (C46) | Branch mode only |
| Branch page and branch table | shared refs only; user refs never create rows |
| Webhooks | `push` for shared refs; `pull_request` and review events per change PR; none for user refs or cloud heads |
| Actions | §3.4; the stack context of C40 is unchanged |
| Drafts page | "Your drafts" (v1.x, B23), rendered from `Cloud.GetWorkspace` |
| Repo settings | "Branchless" toggle and the `shared_refs` list, stored in asc through `RepoAdmin` (B17) |

### 4.3 Stages

| Stage | Server ref policy | Cloud |
|---|---|---|
| 0 git remote, 1 mirror | branch mode (Forgejo git is the truth) | off |
| 2–3 (asc truth, bridge) | branchless allowed; the bridge carries shared refs and `refs/pull/*` only | on |
| ≥ 4 | branchless, the default | on |

---

## 5. Config and migration

### 5.1 Keys

| Key | Scope | Default | Meaning |
|---|---|---|---|
| `asc.branchless` | system, global, local, worktree, `-c`, env `ASC_BRANCHLESS` | `true` | client behaviour: cloud upload, `asc create` without a label, push submits the stack. It never changes local git output. |
| `asc.cloud` | same | `auto` | `auto` = on when `asc.branchless`, a server remote advertises `cloud.v1`, and a daemon runs; `on`; `off` |
| `asc.cloud.workspace` | global, local | `default` | `default`, `host` (the hostname) or a name |
| `asc.cloud.maxSyncAge` | global | `14d` | §2.4 |
| `asc.cloud.queueMax`, `asc.cloud.streams`, `asc.cloud.maxChangesetSize` | global | 10000, 2, 1 GiB | §2.2 |
| `asc.push.review` | global, local | `draft` | `draft`, `publish`, `none` (§3.3) |
| server `repos.branchless` | per repo | `true` for new repos | ref policy (§4.1) |
| server `repos.shared_refs` | per repo | §4.1 | globs |
| `asc-serve.toml [cloud]` | server | `hidden_grace_days = 30`, `abandon_days = 180`, `landed_ref_days = 7`, `max_heads_per_workspace = 10000`, `list_others = "write"` | §1.4, §1.5 |

Effective client mode = `asc.branchless` AND the repo's `branchless` (from `RepoConfig`). A branchless client on a branch-mode repo keeps local draft heads and uploads drafts, but push follows the server (shared branch). A branch-mode client on a branchless repo pushes user refs without submitting PRs.

### 5.2 Migration of monorepo

ASC has about 117 server branches and 350 local branches (`asc-profile.md` §3.4; median 0 commits ahead of master, 79 `worktree-agent-*`).

**Server:** `asc serve admin branchless enable <repo> [--dry-run]` runs at or after the Stage 2 flip, in batches of one SQL transaction each (`ref_log.via = branchless-migrate`), and writes a report.

| Server branch | Action |
|---|---|
| `master`, `release/*`, `green/*`, tags | shared |
| head branch of an open PR | kept shared and listed in `legacy_shared` until the PR closes; then converted to a user ref of the PR author |
| fully merged into `master` (`is_ancestor`) | deleted; the old value stays in `ref_log` (PITR, `asc reflog --remote`) |
| unmerged, no open PR | user ref of the last pusher (Forgejo `branch.pusher_id`, else the repo owner), same name |
| `agent/*`, `worktree-agent-*` | user ref of the pusher with a 7-day TTL |
| unmerged commits on converted branches without a change id | `ChangeMeta` with `change = blake3_derive("asc v1 change migrate", cs)[..16]`, `op = Import` (B20) |

Deletions reach Forgejo through the outbox (`post_receive` with deletions), so its branch table matches. Owners see no change on fetch (`origin/<b>` is still advertised to them); other users' `fetch --prune` prints git's ` - [deleted]` lines.

**Client:** local branches stay as they are; upstreams `origin/<b>` keep working for their owner. `asc migrate branchless [--prune-merged] [--json]` (asc-only, undoable through the op log) reports merged and stale branches, deletes merged ones with `--prune-merged`, seeds `draft_heads` from local branch tips that are drafts, and derives change ids with the same function as the server, so both sides agree with no coordination.

---

## 6. MCP

The checklist's built-in MCP decision covers this surface too. The tools reuse the `--json` shapes and the RPC messages, are ACL-scoped and default to read-only:

| Tool | Shape | Mutating |
|---|---|---|
| `asc_drafts` | `asc sl --json` with `--cloud` | no |
| `asc_cloud_status` | `asc cloud status --json` | no |
| `asc_hide`, `asc_unhide` | mutating-verb JSON (stacks §3) | yes, opt-in |
| `asc_push_stack` | `asc submit --json` | yes, opt-in |

---

## 7. Proposed PLAN units

Conventions as PLAN and stacks §6: small units, disjoint owned files, one acceptance command. Branchless code lives in `crates/asc/src/branchless/`, `crates/asc-engine/src/branchless/`, `crates/asc-serve/src/cloud/`.

**Recorded amendments** (one hunk in a file another unit owns):

| Unit and file | Hunk | Made by |
|---|---|---|
| C04 `crates/asc/src/stack/sinks.rs` | call `DraftHeadSink` next to `RewriteSink` | B03 |
| C05 `crates/asc-engine/src/stack/mod.rs` | heads include visible draft heads | B04 |
| U43b `UpdateRefs` step 1, U47 `ListRefs` | call `refpolicy::classify` / `refpolicy::advertise` | B08 |
| U48d expiry worker | user refs with TTL and `landed_ref_days` | B08 |
| O01 GC marks | roots += `cloud_heads`; `gc_purge` | B10 |
| U32 `cli/push.rs` | call `push_map` when the effective mode is branchless | B12 |
| U31/U32 fetch | trigger a cloud pull after fetch | B15 |
| U53b daemon scheduler | register the uploader and sync tasks | B14 |
| C08 `cli/sl.rs` | `--cloud`, `--user` | B16 |
| U41 `repo.proto` | §2.5 additive fields | B07 |
| U69 admission | class `background` | B09 |

| ID | Title | Crate / repo | Depends | Owns | Accept |
|---|---|---|---|---|---|
| B01 | Config keys and effective-mode resolver (`asc.branchless`, `asc.cloud*`, `asc.push.review`, env) | asc | U20, U50 | `crates/asc/src/branchless/{mod.rs,mode.rs}`, `crates/asc/tests/branchless_mode.rs` | `cargo test -p asc --test branchless_mode` |
| B02 | Local `draft_heads`, `cloud_queue`, `cloud_state`, `cloud_base` tables | asc-state | U17, C02 | `crates/asc-state/migrations/0203_drafts.sql`, `crates/asc-state/src/drafts.rs` | `cargo test -p asc-state drafts::` |
| B03 | `DraftHeadSink`: head and queue maintenance on every new or rewritten changeset | asc | B02, C04 | `crates/asc/src/branchless/heads_sink.rs`, `crates/asc/tests/branchless_heads.rs` | `cargo test -p asc --test branchless_heads && cargo test -p asc-compat --test compat` |
| B04 | Visible set, hide and unhide rules, obsolete heads | asc-engine | B02, C05, U14b | `crates/asc-engine/src/branchless/visible.rs` | `cargo test -p asc-engine branchless::visible` |
| B05 | `asc hide`, `asc unhide` (op-log undoable) | asc | B04, C07, C15 | `crates/asc/src/stack/cli/hide.rs`, `crates/asc/tests/branchless_hide.rs` | `cargo test -p asc --test branchless_hide` |
| B06 | Local GC of hidden un-uploaded drafts | asc-state, asc | B04, C03, U40b | `crates/asc/src/branchless/local_gc.rs`, `crates/asc/tests/branchless_gc.rs` | `cargo test -p asc --test branchless_gc` |
| B07 | Proto `cloud.proto` and the `repo.proto` amendment | asc-rpc | U41, C17 | `proto/asc/v1/cloud.proto`, `crates/asc-rpc/src/cloud.rs` | `cargo test -p asc-rpc cloud::` |
| B08 | Server ref policy: `shared_refs`, user refs, advertisement, owner authz, no outbox fan-out, TTL | asc-serve | U43b, U47, U48d, U44 | `crates/asc-serve/src/refpolicy.rs`, `crates/asc-serve/migrations/{postgres,sqlite}/0203_branchless.sql`, `crates/asc-serve/tests/refpolicy.rs` | `cargo test -p asc-serve --test refpolicy` |
| B09 | Cloud workspace tables and `GetWorkspace`/`UpdateWorkspace`/`ListWorkspaces` (CAS, idempotent) | asc-serve | B07, C18, U69 | `crates/asc-serve/src/cloud/{mod.rs,rpc.rs}`, `crates/asc-serve/migrations/{postgres,sqlite}/0204_cloud.sql` | `cargo test -p asc-serve cloud::rpc` |
| B10 | Expiry worker, GC roots, `Purge` | asc-serve | B09, O01 | `crates/asc-serve/src/cloud/expiry.rs` | `cargo test -p asc-serve cloud::expiry` |
| B11 | Cloud sync engine (three-way merge, CAS retry) | asc-engine | B04, B07 | `crates/asc-engine/src/branchless/sync.rs` | `cargo test -p asc-engine branchless::sync` |
| B12 | Client push mapping (user ref + per-change AGit in one `UpdateRefs`, git-identical output) | asc | B01, B08, C24, U32 | `crates/asc/src/branchless/push_map.rs`, `crates/asc/tests/branchless_push.rs` | `cargo test -p asc --test branchless_push` |
| B13 | Upload pipeline (closure, `HasObjects`, `PutObjects`, `Ingest`, demotion), bounded | asc-engine | B02, C23, U31 | `crates/asc-engine/src/branchless/upload.rs` | `cargo test -p asc-engine branchless::upload` |
| B14 | Daemon uploader and scheduler task (backoff, offline probe, no-daemon path) | asc | B13, B11, B09, U53b | `crates/asc/src/daemon/cloud.rs`, `crates/asc/tests/branchless_daemon.rs` | `cargo test -p asc --test branchless_daemon` |
| B15 | Cloud pull on fetch and `Watch KIND_CLOUD` | asc | B11, B14, C49 | `crates/asc/src/branchless/pull.rs`, `crates/asc/tests/branchless_pull.rs` | `cargo test -p asc --test branchless_pull` |
| B16 | `asc cloud status\|sync\|join\|list\|purge`, `asc sl --cloud --user` | asc | B14, B15, C08 | `crates/asc/src/stack/cli/cloud.rs`, `crates/asc/tests/branchless_cloud_cli.rs` | `cargo test -p asc --test branchless_cloud_cli` |
| B17 | Forgejo: repo setting (branchless, `shared_refs`), no branch rows or push events for user refs, draft PR default | forgejo | K07, K12, C34, B08 | `routers/web/repo/setting/asc_branchless.go`, `services/ascsync/branchless.go` | `go test ./services/ascsync/... ./routers/web/repo/setting/... -run TestAscBranchless` |
| B18 | `asc serve admin branchless enable` (classify, convert, `legacy_shared`, report) | asc-serve | B08, B20, U47, U66 | `crates/asc-serve/src/admin/branchless.rs`, `crates/asc-serve/tests/branchless_migrate.rs` | `cargo test -p asc-serve --test branchless_migrate` |
| B19 | `asc migrate branchless` (report, `--prune-merged`, seed heads, derive ids) | asc | B03, B04, B20 | `crates/asc/src/branchless/migrate.rs`, `crates/asc/tests/branchless_migrate.rs` | `cargo test -p asc --test branchless_migrate` |
| B20 | Derived change id for migrated and meta-less drafts | asc-core | C01 | `crates/asc-core/src/change_derive.rs` | `cargo test -p asc-core change_derive::` |
| B21 | Compat suite in both modes; `asc-sim` scenario (offline, reconnect, two machines, concurrent CAS) | tests, asc-sim | B12, B14, U25, O08 | `tests/compat/tests/modes.rs`, `crates/asc-sim/src/scenarios/cloud.rs` | `cargo test -p asc-compat --test modes && cargo test -p asc-sim cloud` |
| B22 | MCP tools of §6 | asc | B16, MCP units (design pending) | `crates/asc/src/mcp/tools_branchless.rs` | `cargo test -p asc mcp::tools_branchless` |
| B23 | Forgejo "Your drafts" page (v1.x) | forgejo | B09, K06 | `routers/web/user/asc_drafts.go`, `templates/user/asc_drafts.tmpl` | `go test ./routers/web/user/... -run TestAscDrafts` |
| B24 | `asc doctor --workflows`: list `on: push` triggers on non-shared branches | asc | U40, B01 | `crates/asc/src/doctor/workflows.rs`, `crates/asc/tests/doctor_workflows.rs` | `cargo test -p asc --test doctor_workflows` |
| B25 | Benchmark scenarios: commit latency with upload, time to visible, push of an uploaded stack | bench | B14, O05 | `bench/scenarios/branchless.toml` (recorded amendment: O05 owns `bench/**`) | `cargo run -p asc-bench --release -- --suite quick --only branchless` |

Required tests:
- **B03/B21:** the full compat suite is byte-identical with `asc.branchless` true and false, under `git` and `asc`.
- **B04:** the visible set equals a naive oracle on random DAGs; hiding a mid-stack change hides its descendants and re-exposes the parent; evaluating visibility issues no tag lookups and no walk below the trunk.
- **B08:** a non-owner cannot push or list another user's ref; `ListRefs` for one user is O(shared + own), checked with 100k user refs from other users; user-ref updates write no outbox `post_receive`; `REF_NOT_SHARED` for a non-shared tag.
- **B09:** a concurrent `UpdateWorkspace` gets `VERSION_CONFLICT`; a retried `op_id` returns the stored result.
- **B10:** hidden heads expire after the grace period and their objects are swept by two marks; a head with an open PR is never auto-hidden; a purged object is swept at the next run.
- **B12:** the output of `git push origin feat`, `-u`, `--force-with-lease`, `--delete` and a rejected push equals real git's against a git server, after stripping `remote:` lines; a per-change rejection moves nothing.
- **B13/B14:** daemon RSS stays within budget while uploading a 1 GiB stack; a killed upload resumes with `HasObjects`; offline commits never block; the no-daemon path uploads nothing.
- **B11/B21:** two machines in one workspace converge with no lost heads after 1,000 random interleaved operations.
- **B18:** a dry run over an ASC-shaped fixture (117 branches) reports the classification of §5.2; applying it twice is a no-op.

---

## 8. Edits needed in SPEC.md, PLAN.md and the checklist (not made here)

- **§0:** remove "write-behind uploads" from "Out of scope"; add decision 13 (branchless, BD1–BD12).
- **§2.7 ref scope and §6.1 branch scope:** replace "developer branches pushed for review" and "server-held drafts are opt-in, never automatic" with user refs (BD4) and commit cloud (BD6).
- **§3.6:** GC roots gain `cloud_heads`; add `gc_purge`.
- **§6.3:** replace "Nothing is uploaded before `asc push` [fix J1: no write-behind]" with §2 of this file. **§6.4:** add the offline queue behaviour.
- **§7:** add the `Cloud` service and the `repo.proto` fields of §2.5. **§8.2:** add `cloud_ws`, `cloud_heads`, the `repos` columns and `gc_purge`. **§8.3 step 1 and §8.8:** the ref policy and user-ref TTL. **§9.1:** the daemon now uploads drafts (remove "Upload anything before `asc push`"). **§9.2:** add the local tables of §1.2.
- **§11.1:** document the keys of §5.1. **§11.2:** `push` and `fetch` rows gain the branchless notes of §3.2; add `asc hide`, `unhide`, `cloud …` and `migrate branchless` to the asc-only verbs. **§11.3:** cheat sheet line.
- **§13:** add the branchless rows of §4.2–§4.3 and B17/B23 to the patch stack. **§15.2:** replace "Secrets in drafts never leave the machine before `asc push`" with BD9 plus `asc.cloud=off` and `Purge`. **§16.3:** add the `background` class. **§17.1:** compat runs in both modes.
- **PLAN:** add B01–B25 with the dependencies above, and record the amendments table of §7.
- **Checklist:** record the defaults chosen here once the user confirms them: push creates draft PRs (`asc.push.review = draft`), and listing other users' drafts needs repo write.
