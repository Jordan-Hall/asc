# Branchless by default

Status: design for the checklist decision "branchless by default" (2026-10-22, `scion-checklist.md`). It is not normative until it is folded into SPEC.md and PLAN.md; §8 lists the edits those files need. Review fixes are in the same file; rejected or adjusted review items are listed under "Review notes" at the end. It builds on `stacks.md` (SPEC §20): `ChangeMeta`, the `changes` index, `change_versions`, the `Changes` service and D1–D12 are used here and not restated. In PLAN the stacks.md S-units are numbered C-units (S24 = C24); this file uses the PLAN ids.

Inputs: the Scion article (refs discouraged except the main ref (`master` here) and release branches; changesets server-side and visible at once; refs are a single-row CAS separate from changeset creation), Sapling and Commit Cloud (`sync.py`, `workspace.py`, `hide`, smartlog), jj (visible heads, anonymous branches, `refs/jj/keep`, 2-week GC), git-branchless (keepalive refs, `submit`), Gerrit (`refs/for`, `refs/changes`), Mercurial infinitepush, Piper/CitC, git `reftable` and `hideRefs` (research of 2026-10-22).

Fixed rules this design obeys: git commands keep git's exact semantics, output and exit codes; the default branch is `master`; there is no IDE git plumbing; there is no LFS; memory is bounded and nothing scans the whole repo; local-only mode keeps working.

---

## 0. Decisions

| # | Decision | Why |
|---|---|---|
| BD1 | **A draft is a non-immutable changeset that has a `ChangeMeta`** (stacks §2.2). Nothing names it except its change id and changeset id. A branch is never needed. | Scion and Sapling: the draft/public boundary decides what is work in progress, not a ref. |
| BD2 | **Visibility is a set of draft heads, not refs.** Local: `state.db.draft_heads`. Server: `cloud_heads` per user cloud workspace. Visible drafts are the draft ancestors of visible heads. | jj's view and Commit Cloud's heads. Unlike git-branchless, no keepalive refs appear in `for-each-ref`, and advertisement cost does not grow with drafts (the infinitepush lesson). |
| BD3 | **A local branch is a real local ref** (`state.db.refs`, `refs/heads/<b>`), exactly as today. In branchless mode it is a *label*: it is never a shared server ref. | Every local git command then behaves and prints exactly as git, in both modes (`On branch X`, `HEAD detached at`, `for-each-ref`, turbo's `HEAD`). |
| BD4 | **Shared refs are only the patterns in the repo's `shared_refs`** (default `refs/heads/master`, `refs/heads/release/*`, `refs/heads/green/*`, `refs/tags/*`). Every other pushed `refs/heads/<n>` becomes a **user ref** `refs/users/<owner-id>/heads/<n>` (`<owner-id>` is the numeric Forgejo user id, never the login; the login is resolved only for display and for explicit refspecs, so a rename breaks nothing): one CAS row in the existing `refs` table, advertised only to its owner (as `refs/heads/<n>`). A plain git push (`ImportGitPush`) of such a branch becomes a user ref too (§4.1). | Scion principle 6 holds: no global state per draft. The checklist's "user-scoped branches". Git push/fetch semantics stay intact for the owner. |
| BD5 | **`git push origin feat` in branchless mode = user-ref CAS plus submit of the stack** in one `UpdateRefs`: `refs/heads/feat` (user ref) plus one `refs/for/<trunk>/asc-<change>` per change of `trunk..feat` (stacks D8, C24). PRs are created as **drafts** (`asc.push.review`). Output is git's push output for `feat`. | The checklist line "pushing a branch submits its stack as draft changes". Rejections, `-u`, `--force-with-lease` and `--delete` keep git's semantics because the user ref is a real CAS row. |
| BD6 | **Commit cloud:** when online with a daemon, each new changeset and its `ChangeMeta` are uploaded asynchronously (`HasObjects`, `PutObjects`, `Ingest`), then the head set is updated with a single-row CAS (`Cloud.UpdateWorkspace`). Nothing waits for it. Upload is want-only (explicit ids, no have/want negotiation, no packs) and idempotent. **Upload is gated (§2.7):** never on a public repo, only when the repo's `cloud` flag is on, and only after the pre-upload ignore and secret check. | Scion: "nothing to push". `push`/`submit` then find nothing to upload. Local-only and offline work are unchanged. |
| BD7 | **No local git command changes its behaviour or output with the mode.** The compat suite runs the local commands in both modes and must produce byte-identical results; the byte-identical claim covers local commands only. `push` (and the `fetch` cloud pull) are the two commands with a remote effect that depends on the mode: what the server stores for a non-shared push, whether the push submits PRs, whether tags are accepted, and whether drafts upload in the background. Their exit codes, stderr and stdout for the ref lines stay git's. This is a recorded amendment to stacks D4 (§8). | stacks D4 for local commands; the checklist decision (`scion-checklist.md`, "Decision: branchless by default", line 180) for the remote effect of `push`. |
| BD8 | **Hidden is never deleted at once.** `asc hide` (and rewrites, `asc abandon`, cloud removal) drop heads; objects stay addressable by id. Server: hidden heads stay GC roots for 30 days; visible heads untouched for 180 days with no open PR are auto-hidden. Local: hidden drafts have their own retention, `asc.draft.hiddenDays` (default 90), independent of the op-log cap of stacks D6 (1,000 ops or 90 days, whichever comes first). | Sapling hide/unhide, jj's 2-week GC, Commit Cloud's age-based omission. Recovery is always `asc unhide <id>` within the grace period. |
| BD9 | **Scope:** drafts are owned by the authenticated Forgejo user (stored by id). Objects are readable by id by anyone with repo read (the namespace rule, SPEC §15.1, unchanged), which is why upload is off on public repos and guarded by a secret check (§2.7). Listing another user's cloud workspace needs repo write. Only the owner (or an admin, for hide/delete) changes a workspace or a user ref. The owner may purge their own unsubmitted drafts; an admin may purge any. | Scion's "available everywhere" for content, while the listing of work in progress stays among collaborators. This reverses SPEC §15.2 "secrets in drafts never leave the machine"; §2.7 is the compensating control. |
| BD10 | **Two switches.** Client `asc.branchless` (bool, default `true`; system/global/local/worktree/`-c`/env) for client behaviour, and the server repo policy `repos.branchless` (default `true` for new repos and after migration) for ref policy. Per-user opt-out is the client key; per-repo opt-out is the server setting. | The checklist: "switch it off per repo or per user". The server decides what a ref is; a client cannot create shared refs by configuration. Cloud upload needs the server's `repos.branchless` and `repos.cloud` (§5.1); a client key alone never uploads to a repo that has it off. |
| BD11 | **Actions and webhooks fire only for shared refs and PRs.** User refs emit no Forgejo push event and no branch row. CI for drafts runs on `pull_request` events of the per-change PRs (whether Forgejo Actions fires them for draft PRs is unverified; spike B27, §3.4). | No global fan-out per draft. ASC's workflows already use `pull_request` plus `push: [master]`. |
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
cloud_heads(ns BLOB, owner BIGINT /*Forgejo user id*/, name TEXT, cs BLOB, change BLOB /*derived by the server from m/, never client-supplied*/, labels TEXT /*json, validated, display only*/,
            added BIGINT, hidden_at BIGINT NULL, PRIMARY KEY(ns, owner, name, cs))
  + INDEX cloud_heads_hidden (ns, hidden_at), INDEX cloud_heads_added (ns, added)
repos += branchless BOOL, cloud BOOL /*default = branchless; owners can switch cloud off alone*/, shared_refs TEXT /*json globs*/, legacy_shared TEXT /*json names, migration only*/
user_ref_meta(ns BLOB, owner BIGINT, name TEXT, landed_at BIGINT NULL, PRIMARY KEY(ns, owner, name)) + INDEX (ns, landed_at)
cloud_usage(ns BLOB, owner BIGINT, bytes BIGINT, workspaces INT, PRIMARY KEY(ns, owner))   -- quota accounting (§2.7)
-- user refs: rows in the existing refs/ref_log tables (SPEC §3.4), name = refs/users/<owner-id>/heads/<n>; user_ref_meta holds only the landed marker
```

The draft index by change id is the existing `changes` table and `changes_by_change` index (stacks §4.4): `Ingest` already writes a row for every changeset whose closure carries a valid `m/` object. `cloud_heads` adds only the visible set. Stack shape stays derived from the graph (stacks D3).

### 1.4 GC roots and expiry

| Root or rule | Local | Server |
|---|---|---|
| Visible heads | `draft_heads` with `hidden_at IS NULL`: their objects stay in `store/` until uploaded, then demote to `cache/` (as after push, SPEC §6.3 step 5) | every `cloud_heads.cs` (visible or hidden within grace) is a GC root (amends SPEC §3.6) |
| Hidden heads | kept for `asc.draft.hiddenDays` (90) after `hidden_at`, independent of the op log; then `asc gc` deletes un-uploaded objects only they reach. `asc undo` of an op whose head expired fails with "objects expired" and changes nothing | rows with `hidden_at < now − hidden_grace_days` (30) are deleted by the expiry worker; the objects become collectable by O01's two-mark sweep |
| Stale visible heads | none (local work is never auto-hidden) | `added < now − abandon_days` (180) and no open PR for any change in the stack → `hidden_at = now` |
| Submitted versions | – | `change_versions` (stacks §4.4), unchanged |
| User refs | – | ordinary refs (roots); `agent/*` user refs keep their TTL (SPEC §8.8); `user_ref_meta.landed_at` is set in the land transaction (§1.4a) and the ref is deleted `landed_ref_days` (7, 0 disables) later |
| Pull window | – | heads older than `asc.cloud.maxSyncAge` (14 days) are not pulled to new machines; `asc cloud list --all` shows them |
| Secret purge | – | `asc cloud purge <change>` (owner for their own unsubmitted drafts, repo admin for any): deletes the heads, lists the objects in `gc_purge` and queues `cache_evict` for every cache tier (§2.7); O01 sweeps a purged object at its next run if no ref or head reaches it, without the 14-day age rule |

The worker runs hourly in batches of 1,000 rows using the indexes `cloud_heads_hidden`, `cloud_heads_added` and `user_ref_meta (ns, landed_at)`; it never scans `changes` or `refs`.

**1.4a Bounded lookups.**
- *Landed marker.* When a land (`UpdateRefs(LAND)`) succeeds, the same transaction looks at the user refs of the landing author only (at most `max_user_refs`, 1,000 per user) and, for each, checks the change ids of `trunk..tip` (at most the stack-length cap of stacks §2.2) against `land_map`. If all landed, it sets `landed_at`. Nothing walks other users' refs or the whole `refs` table.
- *`push --delete` reachability.* For each change in the deleted ref's range (stack-length cap), the server asks whether it is reachable from the owner's other heads: the owner's `cloud_heads` (at most 10,000) and the owner's other user refs (at most 1,000), by `is_ancestor` with skip lists. The set is capped at 11,000 tips; if the cap is exceeded the PR is left open and the `remote:` line says so. It never reads another owner's heads.

### 1.5 Scoping and access

| Action | Owner | Repo write | Repo read | Repo admin |
|---|---|---|---|---|
| Read draft objects by id (`GetObjects`, `ResolveChanges`, `Stack`, `asc stack get <change>`) | yes | yes | yes | yes |
| List a cloud workspace (`Cloud.GetWorkspace`, `asc sl --cloud --user u`) | yes | yes | no | yes |
| Update a workspace (add, remove, hide) | yes | no | no | hide only |
| Push or delete a user ref | yes | no | no | delete only |
| Fetch another user's ref by explicit refspec `refs/users/<login>/heads/<n>` (the login is resolved to the id on each request) | yes | yes | no | yes |
| Purge a draft (`Cloud.Purge`) | own unsubmitted drafts | no | no | any |
| Receive `Watch KIND_CLOUD` events | own workspaces only | no | no | no |

---

## 2. Commit cloud

### 2.1 Upload pipeline (daemon)

1. Take the oldest `pending` row of `cloud_queue` whose `next_at ≤ now`. Run the guard of §2.7 on its closure first (ignored paths, denied paths, secret scan); a held changeset is not uploaded.
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
| Server workspaces | `[cloud] max_workspaces_per_user` 16 per repo; workspace names match `[A-Za-z0-9._-]{1,64}` | `CLOUD_WORKSPACES_LIMIT` / `INVALID_ARGUMENT` |
| Server user refs | `[cloud] max_user_refs` 1,000 per user per repo | `USER_REFS_LIMIT`; the user deletes refs |
| Server bytes | `[cloud] max_user_bytes` per user per repo, charged at `PutObjects`/`Ingest` (§2.7) | `QUOTA_EXCEEDED` |

### 2.3 Retry, offline and modes

| Situation | Behaviour |
|---|---|
| Transient error (`UNAVAILABLE`, `RESOURCE_EXHAUSTED`, `DEADLINE_EXCEEDED`) | backoff with full jitter from 1 s to 5 min, unbounded attempts, honouring `retry-after-ms` |
| Permanent error (`INVALID_OBJECT`, `PERMISSION_DENIED`, `PATH_POLICY`) | `state = failed`, `last_error` kept, retried only when the head changes or on `asc cloud sync` |
| Offline (connect fails) | the queue pauses; the daemon probes `Ping` with the same backoff; commands never block or print anything |
| No daemon (`ASC_NO_DAEMON`, CI jobs, tests) | nothing uploads in the background; `push`, `submit` and `asc cloud sync` upload in-process |
| local-only, bucket remote, git remote, mirror-mode server, server without the `cloud.v1` feature, a repo with `branchless` or `cloud` off, or a public repo | cloud is off; draft heads stay local |
| `asc.cloud=off` or `asc.branchless=false` | cloud is off |
| Held by the guard (§2.7) | `state = held`; `asc cloud status` shows the reason; `asc cloud release <cs>` uploads it anyway |

### 2.4 Multi-machine sync (Commit Cloud algorithm)

1. Upload local queued heads (§2.1 steps 1–4).
2. `GetWorkspace(since = cloud_state.version, max_age = maxSyncAge)` returns the remote heads.
3. Three-way merge against `cloud_base`: union of heads; a head removed on one side since the base is removed on the other (locally it gets `hidden_at`, so it is recoverable); a remote head that has a local head in its `preds` closure replaces it.
4. `UpdateWorkspace(base_version, add, remove)`. On `VERSION_CONFLICT`, repeat from step 2, at most 3 times, then report it in `asc cloud status`; the next tick retries.
5. Pull the metadata of new remote heads (`Changes.Stack`, `LogEntry`s only, stopping at immutable changesets). Files stay lazy.

Rules: sync never moves `HEAD`, a branch or the working copy. A checked-out change that became obsolete is shown by `asc sl`, and the user moves with `asc next` or `asc sync`. Labels are synced as display text only and never become `refs/heads/*` on another machine (git's branches are local). The daemon syncs every 5 minutes, on `Watch` events of kind `KIND_CLOUD` (sent to the workspace owner only), and when `git fetch` or `asc sl` asks it to. That ask is a non-blocking request to the daemon; with no daemon nothing happens (§3.2). `asc sl --cloud` and `asc cloud sync` run the sync in-process and report failures, because they are asc-only verbs.

### 2.5 RPCs (`proto/asc/v1/cloud.proto`, same port)

```proto
service Cloud {
  rpc GetWorkspace(GetWorkspaceRequest) returns (GetWorkspaceResponse);          // cheap
  rpc UpdateWorkspace(UpdateWorkspaceRequest) returns (UpdateWorkspaceResponse); // background, x-asc-op-id
  rpc ListWorkspaces(ListWorkspacesRequest) returns (ListWorkspacesResponse);    // cheap, paged
  rpc Purge(PurgeRequest) returns (PurgeResponse);                               // admin only
}
message CloudHead { bytes cs = 1; bytes change = 2; /* response only; the server derives it from m/ and ignores the client value */
  repeated string labels = 3; /* <= 8, each <= 64 bytes, [A-Za-z0-9._/-], else INVALID_ARGUMENT */ int64 added = 4; int64 hidden_at = 5; }
message GetWorkspaceRequest { RepoRef repo = 1; string owner = 2; /* empty = caller */ string name = 3;
  uint64 since_version = 4; int64 max_age_s = 5; bool include_hidden = 6; string page_token = 7; }
message GetWorkspaceResponse { uint64 version = 1; repeated CloudHead heads = 2; repeated bytes removed = 3;
  uint32 omitted = 4; string next_page_token = 5; }
message UpdateWorkspaceRequest { RepoRef repo = 1; string name = 2; uint64 base_version = 3;
  repeated CloudHead add = 4; repeated bytes remove = 5; repeated bytes hide = 6; repeated bytes unhide = 7; }
message UpdateWorkspaceResponse { uint64 version = 1; }   // ErrorDetail.code VERSION_CONFLICT carries the current version
// repo.proto, additive: Version.features += "cloud.v1", "branchless.v1"; ErrorDetail.code += VERSION_CONFLICT,
//   CLOUD_HEADS_LIMIT, CLOUD_WORKSPACES_LIMIT, USER_REFS_LIMIT, QUOTA_EXCEEDED, REF_NOT_SHARED; RepoConfigResponse += branchless, cloud, shared_refs, visibility; WatchEvent kind += KIND_CLOUD.
```

### 2.6 Budgets (benchmark gate, O05)

| Metric | Target |
|---|---|
| `commit` latency with the daemon uploading | unchanged within noise (the enqueue is one row in the existing transaction) |
| Time from commit to visible on the server, ≤ 10 changed files, online | p50 < 2 s, p95 < 10 s |
| `push` of an already uploaded stack | one `HasObjects` batch plus one `UpdateRefs` |
| `UpdateWorkspace` | O(changed heads), one row CAS plus head rows |
| `ListRefs` for one user | O(shared refs + own user refs); independent of the number of drafts and users |

### 2.7 Secrets, quota and untrusted fields

Draft objects are readable by every repo reader, including CI tokens and cache tiers, so cloud upload is guarded. This replaces SPEC §15.2 "secrets in drafts never leave the machine" with the controls below.

| Control | Rule |
|---|---|
| Public repos | No auto-upload. `RepoConfig.visibility = public` turns the repo's `cloud` flag off. `asc.cloud=on` on a public repo is refused with a message; the user must use `asc push` / `asc submit`. |
| Pre-upload guard | In the client, before step 2 of §2.1: (a) paths ignored by `.gitignore`/`.ascignore` and the default deny list `asc.cloud.denyPaths` (`.env*`, `*.pem`, `*.key`, `id_*`, `*credentials*`) are held; (b) a bounded secret scan of added blobs of at most 1 MiB each (known token patterns and high-entropy assignments) holds the changeset. Held changesets stay local until `asc cloud release`. `push`/`submit` apply the same guard unless `--no-secret-check` is given. |
| Owner purge | `Cloud.Purge` by the owner works on drafts that have no PR, no `change_versions` row and no shared-ref ancestry. An admin can purge any. |
| Cache eviction | `Purge` writes `gc_purge` and, in the same transaction, `cache_evict(ids)` outbox rows. The outbox worker (U43c) sends the ids to every configured cache tier (CI host cache, `cacheproxy`, bucket/CDN cache) until each acknowledges; `asc cloud purge` reports the unacked tiers. Copies already fetched to client machines cannot be recalled; the command says so. |
| Quota | `PutObjects` and `Ingest` charge bytes to the user's `cloud_usage` and to the Forgejo quota API, because user-ref pushes and cloud ingest skip pre-receive. User-ref updates through `UpdateRefs`/`ImportGitPush` run the same check. Workspaces per user and user refs per user are capped (§2.2). |
| Untrusted fields | `CloudHead.change` is derived by the server from the `m/` object of `cs`. `labels` are validated (§2.5). Forgejo B23 and MCP `asc_drafts` render labels and messages as escaped text, and MCP marks them untrusted data (mcp.md "Untrusted content"). `Watch KIND_CLOUD` goes to the owner only. |

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
| `push origin feat` (non-shared name) | User-ref CAS plus submit of `trunk..feat` (§3.3); remote effect per the stacks D4 amendment (§8) | Creates or updates the server branch (git) |
| `push origin master` / `release/x` / tags in `shared_refs` | Same: shared-ref CAS, branch protection, `--land`/`-o land` | Same |
| `push origin <tag>` not in `shared_refs` | `! [remote rejected] <tag> -> <tag> (asc: ref not shared in this repository)`, exit 1 (a remote-effect difference, covered by the stacks D4 amendment in §8; `asc.branchless=false` or `repos.branchless=false` restores git) | Same as git |
| `push --delete feat` | Deletes the user ref; closes the open PRs of changes that no other user ref or cloud head of the owner reaches | Deletes the server branch |
| `push --force`, `--force-with-lease` | CAS on the user ref with git's rules; the PRs get new versions (lease of stacks §4.2) | Same as git |
| `push -u`, `push` with `push.default` | Same (upstream `origin/feat`) | Same |
| `push --all` | User refs only: each non-shared local branch becomes a user ref and shared-pattern branches follow their CAS rules, but no `refs/for` submit and no PR is created (ASC has about 350 local branches). The output ends with `remote: asc: --all does not submit stacks; use asc submit`. | Same as git |
| `push --mirror` | Rejected before any network call: `fatal: asc: --mirror is not supported on a branchless repository (it would delete refs you cannot see); use --all and --delete`, exit 128 | Same as git |
| `fetch`, `fetch --prune` | Same over the advertised set (shared + own user refs + `refs/pull/*` if configured). The cloud pull is best-effort and runs only in the daemon: `fetch` sends the daemon a non-blocking sync request after its own work. A failed pull or a missing daemon (`ASC_NO_DAEMON`, CI, tests) never changes `fetch`'s exit code, stdout or stderr; the failure appears in `asc cloud status` only. | Same over all server branches |
| `pull` | Same (fetch, then merge or rebase per config) | Same |
| `ls-remote` | Same over the advertised set | Same |
| `log --branches`, `log --all`, `for-each-ref`, `show-ref` | Same: refs only. Anonymous drafts are not refs and do not appear (no `refs/branchless/*`-style keepalives) | Same |
| `rev-parse`, `rev-parse --abbrev-ref HEAD`, `symbolic-ref` | Same | Same |
| `worktree add -b agent/<n>` (Claude Code hooks) | Same; `push -o asc.ttl=7d` makes an expiring user ref | Same; expiring shared branch |
| `asc create` | No label unless a name is given; on a branch the branch advances as with `commit` | Names a branch from `asc.stack.branchTemplate` (C11) |
| `asc submit` | As C24; also updates the user ref of the current branch if it has one | As C24 |
| `asc sl` | Heads = local branches + workspace `HEAD`s + visible draft heads; `--cloud` adds other machines' heads | Heads per stacks §2.2 |

### 3.3 `git push origin feat` in branchless mode

1. Resolve the stack trunk of `feat` (C05). Collect `trunk..feat` first-parent. Commits without a `ChangeMeta` get an id by the C01 import rule (jj header, then Gerrit trailer, then the migration-mode derived id, §5.2).
2. Upload the closure (§2.1; usually already done).
3. One `UpdateRefs` (`--atomic` semantics): `refs/heads/feat` (the server maps it to the user ref; expect = the `origin/feat` value or the lease) plus `refs/for/<trunk>/asc-<change>` per change, bottom to top, with push option `asc.draft` per `asc.push.review`. If the range contains a merge changeset, the push is rejected before any update (stacks §2.2 ends a stack at a merge, and D1 allows one change per review): `! [remote rejected] feat -> feat (asc: stack contains a merge commit; rebase it or submit changes individually)`, exit 1, nothing moved. A rebase that removes the merge, or `asc.push.review=none` (user ref only), avoids the rejection.
4. Any per-change rejection (for example `not allowed to update change <short>`) fails the whole update: `! [remote rejected] feat -> feat (<reason>)`, exit 1, nothing moved.
5. Output is git's for the `feat` line only, plus the server's `remote:` lines (PR URLs), which git also prints verbatim:

```
remote: asc: 3 changes: 2 new draft PRs, 1 updated
remote:   https://forge.example.org/asc/monorepo/pulls/412
To https://forge.example.org/asc/monorepo.git
 * [new branch]      feat -> feat
```

`asc.push.review = none` sends only the user ref; `publish` creates ready PRs. Here `<trunk>` is the default branch (`master` for ASC). The compat suite strips `remote:` lines, as it already does for server-specific text.

### 3.4 Turbo and CI

| Caller | Effect |
|---|---|
| turbo (`.git/HEAD`, `.git/index`, `rev-parse`, `ls-files`, `diff-tree --merge-base`, `status`) | none: all local, branches are real refs |
| `TURBO_SCM_BASE`/`HEAD` in Actions | For a stacked PR `GITHUB_BASE_REF` is the trunk (D8: `base_branch` = trunk), so turbo would diff against `<default_branch>` and include the parent changes. `asc ci checkout` therefore exports `TURBO_SCM_BASE` = the bridge SHA of the PR's `merge_base` (the parent change) and `TURBO_SCM_HEAD=HEAD`, as turbo-ci.md already does when a base is set; turbo's explicit base wins over `GITHUB_BASE_REF`. A workflow that skips `asc ci checkout` gets the `<default_branch>` diff, a safe superset. |
| `asc ci checkout` | checks out `refs/pull/<n>/head`, a changeset id or a shared ref; it never needs a user ref. CI has no daemon, so it uploads nothing. It always fetches `<default_branch>` and writes `origin/<default_branch>` (not only when a base is set), so turbo's `origin/<ref>` fallback (`github_actions_remote_base_ref_fallback`, turbo-ci.md) resolves when `TURBO_SCM_BASE` is unset |
| Workflows `on: pull_request` | fire for every per-change PR. **Unverified for draft PRs:** whether Forgejo Actions fires `pull_request` for a draft PR is not documented in the sources read. Spike B27 tests it on the fork. If it does not fire, B17 patches the fork to fire it, or `asc.push.review` defaults to `publish` on repos that need CI on every push |
| Workflows `on: push: branches: [master]`, `release/*`, `green/*` | unchanged |
| Workflows `on: push` for other branches | no longer fire in a branchless repo; `asc doctor --workflows` lists them (B24) |

---

## 4. Server

### 4.1 Ref policy (applied in `UpdateRefs` step 1 and in `ListRefs`)

| Ref pushed by a client | Branchless repo | Branch-mode repo |
|---|---|---|
| matches `shared_refs` | shared ref: CAS, pre-receive (branch protection), outbox `post_receive`, bridge | same |
| other `refs/heads/<n>` (asc push or `UpdateRefs`) | user ref `refs/users/<owner-id>/heads/<n>`: CAS, owner-only, no pre-receive, no `post_receive`, no bridge, TTL allowed for `agent/*` | shared branch |
| other `refs/tags/*` | rejected `REF_NOT_SHARED` | shared |
| `refs/for/*` | AGit (SPEC §13.5, C34) | same |
| `refs/pull/*`, `refs/snapshots/*` | server-managed, as today | same |
| plain git push through `ImportGitPush` to a non-shared branch | accepted as the pusher's user ref `refs/users/<owner-id>/heads/<n>` (same CAS, same output as git; the imported commits get ids by the C01 rule). No PR is created, because a plain push carries no `refs/for` ref; the pusher runs `asc submit` or pushes with `asc`. AGit pushes (`refs/for/<default_branch>`) work as before. | shared branch |

`shared_refs` defaults to `[refs/heads/<default_branch>, refs/heads/release/*, refs/heads/green/*, refs/tags/*]` plus `land_branches`; it is set per repo through `RepoAdmin` and the Forgejo repo settings (B17). `ListRefs` returns shared refs plus the caller's own user refs renamed to `refs/heads/<n>`; an explicit `refs/users/<login>/` prefix lists another user's (write permission; the login is resolved to the id per request). A user ref and a shared ref can never collide, because the name decides the class.

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
| `asc.cloud` | same | `auto` | `auto` = on when `asc.branchless`, a server remote advertises `cloud.v1` with the repo's `cloud` flag on, and a daemon runs; `on` (still refused on a public repo); `off` |
| `asc.cloud.workspace` | global, local | `default` | `default`, `host` (the hostname) or a name |
| `asc.cloud.maxSyncAge` | global | `14d` | §2.4 |
| `asc.cloud.queueMax`, `asc.cloud.streams`, `asc.cloud.maxChangesetSize` | global | 10000, 2, 1 GiB | §2.2 |
| `asc.push.review` | global, local | `draft` | `draft`, `publish`, `none` (§3.3); remote effect of `push` per the stacks D4 amendment (§8) |
| server `repos.branchless` | per repo | `true` for new repos | ref policy (§4.1) |
| server `repos.shared_refs` | per repo | §4.1 | globs |
| server `repos.cloud` | per repo | = `repos.branchless`; always off on public repos | whether the server accepts cloud upload (§2.7) |
| `asc.draft.hiddenDays` | global | `90` | local retention of hidden drafts (§1.4) |
| `asc.cloud.denyPaths` | global | §2.7 | paths the guard holds |
| `asc-serve.toml [cloud]` | server | `hidden_grace_days = 30`, `abandon_days = 180`, `landed_ref_days = 7`, `max_heads_per_workspace = 10000`, `max_workspaces_per_user = 16`, `max_user_refs = 1000`, `max_user_bytes`, `list_others = "write"` | §1.4, §1.5, §2.7 |

Effective client mode = `asc.branchless` AND the repo's `branchless` (from `RepoConfig`). Cloud upload needs the client key (`asc.cloud` not `off`), the server's `repos.branchless` and `repos.cloud`, and a non-public repo. A branchless client on a branch-mode repo therefore keeps local draft heads only and uploads nothing; push follows the server (shared branch). A branch-mode client on a branchless repo pushes user refs without submitting PRs.

### 5.2 Migration of monorepo

ASC has about 117 server branches and 350 local branches (`asc-profile.md` §3.4; median 0 commits ahead of master, 79 `worktree-agent-*`).

**Server:** `asc serve admin branchless enable <repo> [--dry-run]` runs at or after the Stage 2 flip, in batches of one SQL transaction each (`ref_log.via = branchless-migrate`), and writes a report.

| Server branch | Action |
|---|---|
| `master`, `release/*`, `green/*`, tags | shared |
| head branch of an open PR | kept shared and listed in `legacy_shared` until the PR closes; then converted to a user ref of the PR author by B26 |
| fully merged into `master` (`is_ancestor`) | deleted; the old value stays in `ref_log` (PITR, `asc reflog --remote`) |
| unmerged, no open PR | user ref of the last pusher (Forgejo `branch.pusher_id`, else the repo owner), same name |
| `agent/*`, `worktree-agent-*` | user ref of the pusher with a 7-day TTL |
| unmerged commits on converted branches without a change id | `ChangeMeta` by the C01 import rule with migration mode on: a jj `change-id` header ranks first, a Gerrit `Change-Id` trailer second (stacks.md git-import row), and only when neither exists the derived id `blake3_derive("asc v1 change import", cs)[..16]`, `op = Import`. This adds one tier to C01 and no separate domain (amendment in §7) |

Deletions reach Forgejo through the outbox (`post_receive` with deletions), so its branch table matches. Owners see no change on fetch (`origin/<b>` is still advertised to them); other users' `fetch --prune` prints git's ` - [deleted]` lines.

**Client:** local branches stay as they are; upstreams `origin/<b>` keep working for their owner. `asc migrate branchless [--prune-merged] [--json]` (asc-only, undoable through the op log) reports merged and stale branches, deletes merged ones with `--prune-merged`, seeds `draft_heads` from local branch tips that are drafts, and derives change ids with the same C01 rule as the server, so both sides agree with no coordination.

---

## 6. MCP

The checklist's built-in MCP decision covers this surface too. Tools are rows in `crates/asc-mcp/tools.toml` (mcp.md M03), reuse the `--json` shapes and RPC messages, are ACL-scoped and obey M-D4 and M-D9. Branchless adds no new toolset.

| Tool | Toolset (M-D9) | Shape | Mutating | Gate (M-D4) |
|---|---|---|---|---|
| `asc_drafts` | `stack` (opt-in, read) | `asc sl --json` with `--cloud`; labels and messages are returned as untrusted data | no | none |
| `asc_cloud_status` | `stack` | `asc cloud status --json` | no | none |
| `asc_hide`, `asc_unhide` | `write` | mutating-verb JSON (stacks §3); local effect, undoable | yes | `asc.mcp.write=local` |
| `asc_push_stack` | `write` | `asc submit --json`; remote effect, needs confirmation | yes | `asc.mcp.write=remote` |

**Superseded [audit: fold-verification-2]:** SPEC §21.7 and §22.3 use the short names `drafts`, `cloud_status`, `hide` and `unhide`, drop `asc_push_stack` (`submit` covers it) and use one per-workspace write gate instead of `write=local` for hide/unhide. The names above are history.

The default `core` set (11 read tools) is unchanged. `asc_hide`/`asc_unhide` run as child verbs through M10's write path; `asc_push_stack` is hidden unless `write=remote`. Purge is not exposed over MCP.

---

## 7. Proposed PLAN units

Conventions as PLAN and stacks §6: small units, disjoint owned files, one acceptance command. Branchless code lives in `crates/asc/src/branchless/` (`mod.rs` owned by B01), `crates/asc-engine/src/branchless/` (`mod.rs` owned by B04) and `crates/asc-serve/src/cloud/` (`mod.rs` owned by B09). Each later unit adds its own `pub mod` line to the `mod.rs` of its directory; those one-line registrations are recorded amendments below.

**Recorded amendments** (one hunk in a file another unit owns):

| Unit and file | Hunk | Made by |
|---|---|---|
| C04 `crates/asc/src/stack/sinks.rs` | call `DraftHeadSink` next to `RewriteSink` | B03 |
| C05 `crates/asc-engine/src/stack/mod.rs` | heads include visible draft heads | B04 |
| U43b `UpdateRefs` step 1, U47 `ListRefs` | call `refpolicy::classify` / `refpolicy::advertise` | B08a |
| U43b ingest, U48a `Ingest`/`PutObjects`, U48d `ImportGitPush` | call `quota::charge` (cloud and user-ref pushes skip pre-receive) | B09 |
| U48d `ImportGitPush` | non-shared branch maps to the pusher's user ref | B08a |
| U48d expiry worker | user refs with TTL and `landed_at` | B08c |
| U43c outbox | `cache_evict` row kind | B10 |
| O01 GC marks | roots += `cloud_heads`; `gc_purge` | B10 |
| C01 import rule (`crates/asc-core`) | third tier `blake3_derive("asc v1 change import", cs)[..16]` in migration mode, after the jj header and the Gerrit trailer | B18 |
| C34 land transaction | set `user_ref_meta.landed_at` (§1.4a) | B08c |
| U62 `asc ci checkout` | export `TURBO_SCM_BASE` from the PR's `merge_base`; always write `origin/<default_branch>` | B27 |
| O05 `bench/**` (`asc-bench` CLI) | add the `--only <scenario>` filter | B25 |
| `crates/asc/src/branchless/mod.rs` (B01) | `pub mod` lines for `heads_sink`, `push_map`, `pull`, `local_gc`, `migrate` | B03, B12, B15, B06, B19 |
| `crates/asc-engine/src/branchless/mod.rs` (B04) and `crates/asc-engine/src/lib.rs` | `pub mod` lines for `sync`, `upload`, `guard`; `pub mod branchless;` | B11, B13, B28; B04 |
| `crates/asc-serve/src/cloud/mod.rs` (B09) and `crates/asc-serve/src/lib.rs` | `pub mod expiry, purge`; `mod cloud, refpolicy, refauthz, reftl` | B10; B08a, B08b, B08c, B09 |
| M03 `crates/asc-mcp/tools.toml` | rows for the §6 tools | B22 |
| U32 `cli/push.rs` | call `push_map` when the effective mode is branchless | B12 |
| U31/U32 fetch | trigger a cloud pull after fetch | B15 |
| U53b daemon scheduler | register the uploader and sync tasks | B14 |
| C08 `cli/sl.rs` | `--cloud`, `--user` | B16 |
| U41 `repo.proto` | §2.5 additive fields | B07 |
| U69 admission | class `background` | B09 |

| ID | Title | Crate / repo | Depends | Owns | Accept |
|---|---|---|---|---|---|
| B01 | Config keys and effective-mode resolver (`asc.branchless`, `asc.cloud*`, `asc.draft.hiddenDays`, `asc.push.review`, env) | asc | U20, U50 | `crates/asc/src/branchless/{mod.rs,mode.rs}`, `crates/asc/tests/branchless_mode.rs` | `cargo test -p asc --test branchless_mode` |
| B02 | Local `draft_heads`, `cloud_queue`, `cloud_state`, `cloud_base` tables | asc-state | U17, C02 | `crates/asc-state/migrations/0203_drafts.sql`, `crates/asc-state/src/drafts.rs` | `cargo test -p asc-state drafts::` |
| B03 | `DraftHeadSink`: head and queue maintenance on every new or rewritten changeset | asc | B02, C04, U25 | `crates/asc/src/branchless/heads_sink.rs`, `crates/asc/tests/branchless_heads.rs` | `cargo test -p asc --test branchless_heads && cargo test -p asc-compat --test compat` |
| B04 | Visible set, hide and unhide rules, obsolete heads | asc-engine | B02, C05, U14b | `crates/asc-engine/src/branchless/{mod.rs,visible.rs}` | `cargo test -p asc-engine branchless::visible` |
| B05 | `asc hide`, `asc unhide` (op-log undoable) | asc | B04, C07, C15 | `crates/asc/src/stack/cli/hide.rs`, `crates/asc/tests/branchless_hide.rs` | `cargo test -p asc --test branchless_hide` |
| B06 | Local GC of hidden un-uploaded drafts (own retention `asc.draft.hiddenDays`) | asc-state, asc | B04, B01, C03, U40b | `crates/asc/src/branchless/local_gc.rs`, `crates/asc/tests/branchless_gc.rs` | `cargo test -p asc --test branchless_gc` |
| B07 | Proto `cloud.proto` and the `repo.proto` amendment | asc-rpc | U41, C17 | `proto/asc/v1/cloud.proto`, `crates/asc-rpc/src/cloud.rs` | `cargo test -p asc-rpc cloud::` |
| B08a | Server ref policy: `shared_refs`, user-ref classification, advertisement, `ImportGitPush` mapping | asc-serve | U43b, U47, U48d | `crates/asc-serve/src/refpolicy.rs`, `crates/asc-serve/migrations/{postgres,sqlite}/0203_branchless.sql`, `crates/asc-serve/tests/refpolicy.rs` | `cargo test -p asc-serve --test refpolicy` |
| B08b | User-ref authz (owner-only), per-user ref cap, quota on user-ref pushes, no outbox fan-out | asc-serve | B08a, U44 | `crates/asc-serve/src/refauthz.rs`, `crates/asc-serve/tests/refauthz.rs` | `cargo test -p asc-serve --test refauthz` |
| B08c | User-ref TTL, `user_ref_meta.landed_at` and `landed_ref_days` expiry | asc-serve | B08a, U48d, C34 | `crates/asc-serve/src/reftl.rs`, `crates/asc-serve/migrations/{postgres,sqlite}/0205_user_ref_meta.sql`, `crates/asc-serve/tests/reftl.rs` | `cargo test -p asc-serve --test reftl` |
| B09 | Cloud workspace tables and `GetWorkspace`/`UpdateWorkspace`/`ListWorkspaces` (CAS, idempotent), workspace caps, label validation, `quota::charge` | asc-serve | B07, C18, U69 | `crates/asc-serve/src/cloud/{mod.rs,rpc.rs,quota.rs}`, `crates/asc-serve/migrations/{postgres,sqlite}/0204_cloud.sql` | `cargo test -p asc-serve cloud::rpc cloud::quota` |
| B10 | Expiry worker, GC roots, `Purge` (owner and admin), `cache_evict` | asc-serve | B09, O01, U43c | `crates/asc-serve/src/cloud/{expiry.rs,purge.rs}` | `cargo test -p asc-serve cloud::expiry cloud::purge` |
| B11 | Cloud sync engine (three-way merge, CAS retry) | asc-engine | B04, B07 | `crates/asc-engine/src/branchless/sync.rs` | `cargo test -p asc-engine branchless::sync` |
| B12 | Client push mapping (user ref + per-change AGit in one `UpdateRefs`; `--all`, `--mirror`, merge rejection; git-identical output) | asc | B01, B08a, B08b, B13, C24, U32 | `crates/asc/src/branchless/push_map.rs`, `crates/asc/tests/branchless_push.rs` | `cargo test -p asc --test branchless_push` |
| B13 | Upload pipeline (guard, closure, `HasObjects`, `PutObjects`, `Ingest`, demotion), bounded | asc-engine | B02, B28, C23, U31 | `crates/asc-engine/src/branchless/upload.rs` | `cargo test -p asc-engine branchless::upload` |
| B14 | Daemon uploader and scheduler task (backoff, offline probe, no-daemon path, public-repo gate) | asc | B13, B11, B09, U53b | `crates/asc/src/daemon/cloud.rs`, `crates/asc/tests/branchless_daemon.rs` | `cargo test -p asc --test branchless_daemon` |
| B15 | Cloud pull on fetch (daemon only, best-effort) and `Watch KIND_CLOUD` (owner only) | asc | B11, B14, C49 | `crates/asc/src/branchless/pull.rs`, `crates/asc/tests/branchless_pull.rs` | `cargo test -p asc --test branchless_pull` |
| B16 | `asc cloud status\|sync\|join\|list\|release\|purge`, `asc sl --cloud --user` | asc | B14, B15, B10, C08 | `crates/asc/src/stack/cli/cloud.rs`, `crates/asc/tests/branchless_cloud_cli.rs` | `cargo test -p asc --test branchless_cloud_cli` |
| B17 | Forgejo: repo setting (branchless, cloud, `shared_refs`), no branch rows or push events for user refs, draft PR default, draft-PR `pull_request` trigger if B27 requires it | forgejo | K07, K12, C34, B08a | `routers/web/repo/setting/asc_branchless.go`, `services/ascsync/branchless.go` | `go test ./services/ascsync/... ./routers/web/repo/setting/... -run TestAscBranchless` |
| B18 | `asc serve admin branchless enable` (classify, convert, `legacy_shared`, report) and the C01 migration-mode tier | asc-serve | B08a, B08c, C01, U47, U66 | `crates/asc-serve/src/admin/branchless.rs`, `crates/asc-serve/tests/branchless_migrate.rs` | `cargo test -p asc-serve --test branchless_migrate` |
| B19 | `asc migrate branchless` (report, `--prune-merged`, seed heads, derive ids by C01) | asc | B03, B04, C01 | `crates/asc/src/branchless/migrate.rs`, `crates/asc/tests/branchless_migrate.rs` | `cargo test -p asc --test branchless_migrate` |
| B20 | withdrawn: folded into C01 (import derivation, migration-mode tier); see amendments | - | - | - | - |
| B21 | Compat suite in both modes; `asc-sim` scenario (offline, reconnect, two machines, concurrent CAS) | tests, asc-sim | B12, B14, U25, O08 | `tests/compat/tests/modes.rs`, `crates/asc-sim/src/scenarios/cloud.rs` | `cargo test -p asc-compat --test modes && cargo test -p asc-sim cloud` |
| B22 | MCP handlers for the §6 tools; rows in `tools.toml` by amendment | asc, asc-mcp | B16, M03, M10 | `crates/asc/src/mcp/branchless.rs`, `crates/asc/tests/mcp_branchless.rs` | `cargo test -p asc-mcp catalogue:: && cargo test -p asc --test mcp_branchless` |
| B23 | Forgejo "Your drafts" page (v1.x; escapes labels) | forgejo | B09, K06 | `routers/web/user/asc_drafts.go`, `templates/user/asc_drafts.tmpl` | `go test ./routers/web/user/... -run TestAscDrafts` |
| B24 | `asc doctor --workflows`: list `on: push` triggers on non-shared branches | asc | U40, B01 | `crates/asc/src/doctor/workflows.rs`, `crates/asc/tests/doctor_workflows.rs` | `cargo test -p asc --test doctor_workflows` |
| B25 | Benchmark scenarios: commit latency with upload, time to visible, push of an uploaded stack | bench | B14, O05 | `bench/scenarios/branchless.toml` (recorded amendment: O05 owns `bench/**` and gains `--only`) | `cargo run -p asc-bench --release -- --suite quick --only branchless` |
| B26 | `legacy_shared` conversion: when a PR closes, convert its head branch to a user ref of the PR author | asc-serve | B18, B08a, K12, C34 | `crates/asc-serve/src/admin/legacy_shared.rs`, `crates/asc-serve/tests/legacy_shared.rs` | `cargo test -p asc-serve --test legacy_shared` |
| B27 | CI: spike whether Forgejo Actions fires `pull_request` for draft PRs; `asc ci checkout` base for stacked PRs | asc-ci, forgejo | U62, B17, C40 | `crates/asc-ci/src/stack_base.rs`, `crates/asc-ci/tests/stack_base.rs` | `cargo test -p asc-ci stack_base::` (the draft-PR spike result is recorded by the parent) |
| B28 | Pre-upload guard: ignored and denied paths, bounded secret scan | asc-engine | B02 | `crates/asc-engine/src/branchless/guard.rs` | `cargo test -p asc-engine branchless::guard` |

Required tests:
- **B03/B21:** the full compat suite is byte-identical with `asc.branchless` true and false, under `git` and `asc`.
- **B04:** the visible set equals a naive oracle on random DAGs; hiding a mid-stack change hides its descendants and re-exposes the parent; evaluating visibility issues no tag lookups and no walk below the trunk.
- **B08a/b/c:** a non-owner cannot push or list another user's ref; `ListRefs` for one user is O(shared + own), checked with 100k user refs from other users; user-ref updates write no outbox `post_receive`; `REF_NOT_SHARED` for a non-shared tag; a plain git push to a non-shared branch lands as a user ref; the landed marker and the `push --delete` check read only the owner's rows (query-count test).
- **B09:** a concurrent `UpdateWorkspace` gets `VERSION_CONFLICT`; a retried `op_id` returns the stored result; a client-supplied `change` is ignored; bad labels and a 17th workspace are refused; `PutObjects` over quota gets `QUOTA_EXCEEDED`.
- **B10:** hidden heads expire after the grace period and their objects are swept by two marks; a head with an open PR is never auto-hidden; a purged object is swept at the next run; an owner can purge their own unsubmitted draft but not a submitted one or another user's; `cache_evict` is retried until acked.
- **B12:** `push --all` creates no PR; `push --mirror` is rejected; a range with a merge is rejected with nothing moved; the output of `git push origin feat`, `-u`, `--force-with-lease`, `--delete` and a rejected push equals real git's against a git server, after stripping `remote:` lines; a per-change rejection moves nothing.
- **B28/B13/B14:** a `.env` change and a changeset with a planted token are held and not uploaded; nothing uploads on a public repo or when `repos.cloud` is off; daemon RSS stays within budget while uploading a 1 GiB stack; a killed upload resumes with `HasObjects`; offline commits never block; the no-daemon path uploads nothing.
- **B11/B21:** two machines in one workspace converge with no lost heads after 1,000 random interleaved operations.
- **B26:** closing a `legacy_shared` PR converts its branch to the author's user ref once.
- **B15:** a failed cloud pull leaves `fetch`'s exit code and stderr identical to a run with no daemon.
- **B18:** a dry run over an ASC-shaped fixture (117 branches) reports the classification of §5.2; applying it twice is a no-op.

---

## 8. Edits needed in SPEC.md, PLAN.md and the checklist (not made here)

- **stacks.md D4 (stacks.md:24), recorded amendment:** add "except the remote effect of `push` (and the best-effort cloud pull of `fetch`) under `asc.branchless` / `repos.branchless`, per the checklist decision 'branchless by default' (scion-checklist.md:180: pushing a branch submits its stack as draft changes). Local commands are unchanged; `asc.branchless=false` or `repos.branchless=false` restores git's remote semantics."
- **stacks.md git-import row (C01):** add the migration-mode third tier (§5.2).
- **§0:** remove "write-behind uploads" from "Out of scope"; add decision 13 (branchless, BD1–BD12).
- **§2.7 ref scope and §6.1 branch scope:** replace "developer branches pushed for review" and "server-held drafts are opt-in, never automatic" with user refs (BD4) and commit cloud (BD6).
- **§3.5:** "`store/` is never evicted" becomes: never evicted, except objects reachable only from hidden draft heads past `asc.draft.hiddenDays` (B06); un-uploaded objects of visible heads are never evicted. **§3.6:** GC roots gain `cloud_heads`; add `gc_purge`.
- **§6.3:** replace "Nothing is uploaded before `asc push` [fix J1: no write-behind]" with §2 of this file. **§6.4:** add the offline queue behaviour.
- **§7:** add the `Cloud` service and the `repo.proto` fields of §2.5. **§8.2:** add `cloud_ws`, `cloud_heads`, the `repos` columns and `gc_purge`. **§8.3 step 1 and §8.8:** the ref policy and user-ref TTL. **§9.1:** the daemon now uploads drafts (remove "Upload anything before `asc push`"). **§9.2:** add the local tables of §1.2.
- **§11.1:** document the keys of §5.1. **§11.2:** `push` and `fetch` rows gain the branchless notes of §3.2; add `asc hide`, `unhide`, `cloud …` and `migrate branchless` to the asc-only verbs. **§11.3:** cheat sheet line.
- **§13:** add the branchless rows of §4.2–§4.3 and B17/B23 to the patch stack. **§15.2:** replace "Secrets in drafts never leave the machine before `asc push`" with BD9 plus the §2.7 controls (no upload on public repos, guard, owner purge, cache eviction, quota) and `asc.cloud=off`. **§16.3:** add the `background` class. **§17.1:** compat runs in both modes.
- **PLAN:** add B01–B19 and B21–B28 (B20 withdrawn) with the dependencies above, and record the amendments table of §7, including the `mod.rs` registrations, the O05 `--only` flag and the U62 and C01 hunks.
- **Checklist:** record the defaults chosen here once the user confirms them: push creates draft PRs (`asc.push.review = draft`), and listing other users' drafts needs repo write.

---

## Review notes

All 17 review items were accepted. Where the fix differs from the critique's wording:

- Item 5: the secret scan is bounded and pattern-based, not a guarantee; copies already fetched to client machines cannot be recalled (stated in §2.7).
- Item 13: B20 is withdrawn rather than renumbered, so B21-B25 keep their ids.
- Item 15: Forgejo Actions behaviour for draft PRs could not be confirmed (a search found no documentation), so §3.4 marks it unverified and B27 is a spike with a fallback. The stacked-PR base and `origin/<default_branch>` fixes do not depend on it.
- Item 14 (B25): `--only` is recorded as an O05 amendment instead of changing B25's accept command.
