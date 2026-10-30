# Build-out contract

This page is the contract for finishing the features that the README of ASC Hub lists as partial or planned. It fixes the wire
protocol, the data, the configuration, the error codes and who owns which crate or package, so the units that build the features do
not collide. The protocol is in `proto/asc/v1` (new files only; nothing existing changed), the generated Go stubs are in the hub
(`modules/ascproto/ascv1`) and the generated TypeScript is `@asc/asc-client`.

Rules for every unit:

- A feature is "available" in the README table of the hub, and in `docs/STATUS.md` here, only when its code, its tests and its docs
  are in. A feature page below says what "done" means for it.
- No scaffold survives. A service is served or it is not mounted (the router answers `UNIMPLEMENTED` for a method nobody serves), and
  `cargo test -p asc-rpc --test scaffold_registry` keeps the RPC index below in step with the protos and fails if a scaffold list returns.
- Within `asc.v1` the protocol only grows. A unit that needs a field or a method adds it to the proto in the same commit, runs the
  generators below and says so in its commit message; it never renumbers or removes a field.
- No secret is ever logged, put in an error, put in an audit event or returned after it was set. Section [Secrets](#secrets) says how.

## How to regenerate

| Output | Command | Committed |
|---|---|---|
| Rust (prost, tonic) | built by `crates/asc-rpc/build.rs` from `proto/asc/v1/*.proto`; `cargo check -p asc-rpc` | no (build output) |
| TypeScript | `bun run codegen` in `packages/asc-client-ts` (buf and `protoc-gen-es`) | no (`src/gen` is ignored) |
| Relay allowlist | edit `packages/asc-client-ts/src/transport/allowlist.json`, then `bun scripts/gen-allowlist.ts` | yes |
| Go (hub) | in the hub root, with `asc` checked out next to it: `buf generate` (plugins pinned in `modules/ascproto/README.md`) | yes |

Every RPC must be classified exactly once in `allowlist.json` (the IDE relay). Unit 0 classified read-only calls as `read` and
everything that changes state, holds a secret or is for administrators as `never`; a unit that wants a call in the IDE moves it.

## Conventions

**Who is calling.** `asc serve` authorises by the authenticated principal: kind `user`, `agent`, `deploy`, `ci` or `host`. The hub
forwards the signed-in user's own asc token for user-initiated calls (`ascclient.WithUserToken`) and the internal token only for
system work (`WithSystem`); a system call never stands in for a user. Access levels used below: *repo read*, *repo write*,
*repo admin* (as in `Access` in `asc-rpc`), *maintainer* (named by the policy role `maintainer`), *owner admin* (admin of the user,
organisation or repository that owns the resource), *site administrator*.

**Forwarded claim.** A page of the hub calls `asc serve` with the hub's service credential (the host service) and a claim about
the person: what the hub's own permission model decided the person may do on the subject of the call. The claim is four gRPC
metadata entries, signed with a key the hub and the server share (`[auth] forwarded_key_file`, 32 bytes raw or 64 hex digits, mode
0600; without it no claim is accepted and a call carrying one is refused): `x-asc-forwarded-user: <user id>:<login>`,
`x-asc-forwarded-role: read|write|admin`, `x-asc-forwarded-ts: <unix seconds, canonical decimal>` and `x-asc-forwarded-mac`, the
lower-case hex HMAC-SHA256 of `asc-forwarded-v1
<user>
<role>
<subject>
<ts>`. The subject is exactly the name the server
stores, never folded: `owner/name` for a repository (the `RepoRef` name of the request), `org:<name>` and `user:<login>` (lower
case, as `OwnerRef` names are stored), and `instance`. `asc_rpc::forwarded` and `authz/claim.rs` (`subject::*`) define it once.

Rules, all fail closed:

* A claim counts only on a call authenticated as the host service, only for its own subject, and within 60 seconds of the
  server's clock either way (that window is the replay window; the hub signs each call anew).
* A host-service call that carries a claim is bound by it. A claim that is malformed, stale, signed under another key, for another
  subject or too small is `PERMISSION_DENIED`; it is never read as system work. A `read` claim cannot write and a `write` claim
  cannot administer. Without a claim the host service is the system, as before.
* Every repository-scoped RPC checks the claim against the access it needs (`read`, `write` or `admin`). Audit scopes (repository
  admin; organisation owner; site administrator for `instance`; the person's own account), AI keys, storage targets and the vault
  are decided by the claim for the repository, organisation, account or instance they name. The signing keys pages act as the
  person of an account claim (`user:<login>`, `read` to list, `write` to add or revoke).
* Audit events of such a call name the person as the actor and the host service as `on_behalf_of` (the relay); a stored answer
  of a mutating call is bound to the person and the subject as well as to the host principal, so it never replays to another
  person or another target.
* An unusable policy can be repaired by the host service only with an `admin` claim for the repository (logged as
  `policy.repair.forwarded_admin`).

**Authorization answers from the hub (K07).** `asc serve` asks `POST /api/internal/asc/authz` and reuses the answer only briefly
and only in part. The cache key is the credential, the repository and the access; an entry lives at most 5 seconds (a configured
longer lifetime is cut to it; a revocation cannot be seen without asking). Only a plain `read` decision is reused, and what is
reused has no branch rules and no task: a caller that reads those (`decide`) asks every time, every `write` and `admin` call
asks the hub fresh, an Actions task is never kept, and a hub that cannot answer refuses the caller and drops what it held. The
answer's `perm_version` (a fingerprint of the permission facts, equality only) is kept with the decision and a change is logged
as `authz.perm_version_changed`. `authenticate()` (who is this credential, with no repository) is the same endpoint with an empty
`owner` and `repo`: the hub answers `allowed` with `user_id` and `username` when the credential authenticates, so no `read:user`
scope is needed.

**Tokens with a hub.** Behind a hub (`auth.provider = "forgejo"`) the hub's own access tokens, with their scopes, are the credential:
asc stores none of them. An agent token (`ascat_`) is the hub's scoped-token idea applied to one repository, one agent and a budget,
and it is only as good as the person who made it: on every use the hub is asked (`as_user_id`, internal token only, the answer must
name that person) whether they may still read, or for a write still write, the repository; a person who lost access, a hub that
cannot answer, and an answer about someone else all refuse the token (a read is kept for 5 seconds at most). The parallel systems
that remain, and what each is for: the standalone token store (`provider = "token_store"`, `asc serve token ...`, `token_key_file`)
exists for a server with no hub and is unused behind one, as are the SSH keys of that store; the vault key file seals stored
secrets and is not a credential; the MCP HMAC keys seal cursors; `forwarded_key_file` signs the hub's claims. Remove the token store
key from a hub deployment's config: nothing reads it.

**Fail closed.** A handler that cannot decide (policy unreadable, store down, clock skew) refuses. Reading a resource of an owner the
caller cannot administer answers `NOT_FOUND`, so identifiers cannot be probed.

**Mutating calls** carry `x-asc-op-id`; a retried id returns the first answer (the `op_results` table). **List calls** take
`limit` (0 means the stated default, capped) and an opaque `page_token`. Times are unix seconds unless the name ends in `_ms`.

**Bytes and JSON.** Every `bytes` field carries an `(asc.v1.json)` option (the JSON profile prints change ids, paths and ids). The
test `crates/asc-rpc/tests/json_annotations.rs` lists the proto files it checks; a unit that adds a file adds it there.

**Repositories are named by `RepoRef`.** The hub's RPC proxy (`routers/web/repo/asc_rpc_proxy.go`) finds every `RepoRef` in a request
and checks the browser user's read access before forwarding. Methods whose requests name a repository another way (`OwnerRef` with
kind `REPO`, `CrossSearchTarget.repos`) or that hold secrets (`Storage`, `AiProviders`, `Vault`, `Audit`, `AgentTokens`, `Signing`
keys) must never be put on that read allowlist; the hub serves them from its own handlers, which check Forgejo permissions first and
then call asc with the user's token.

**Feature flags.** `Version.features` gains one string per service (named on each page). A client checks it before calling; a server
that lacks the feature answers `UNIMPLEMENTED`.

**Audit.** Every mutating call of every service below appends an audit event in the same transaction (section 3). The action names are
the dotted names on the pages.

**Agents are optional.** The server key `[agents] enabled` (default `false`) and the repository policy `[agents] enabled` (default
`true` once the server allows it) both have to be on; the web UI hides every agent surface behind the existing agent-features switch
(off by default). With agents off, nothing else changes.

## Secrets

Storage credentials (S3 key pairs), AI provider keys and any other stored secret follow one design.

- **Sealed at rest.** XChaCha20-Poly1305 with a per-value random 24-byte nonce. The data key is HKDF-SHA256 over the master key with
  the context `"asc-vault-v1" || kind || owner || row id`, and the same context is the associated data, so a sealed value cannot be
  moved to another owner or row. Stored form: `generation (u32) || nonce || ciphertext || tag`.
- **Master key.** 32 random bytes from the OS RNG in the file named by `[vault] key_file`, created on first start with mode `0600`
  (the process refuses to start the vault when the mode is wider, and on Windows when the ACL admits others). Several generations
  are kept in the file; `Vault.Rotate` adds one, makes it active and re-seals every value in the background; a retired generation
  is dropped when nothing is sealed under it. Keys are zeroised on drop (`zeroize`).
- **Write-only.** A secret is accepted on create and update and never returned. Responses carry a hint (last 4 characters, or first
  and last 4 for an access key id) and a `has_*` flag. Agent token secrets are the one thing generated by the server: shown once in the
  create response, stored as SHA-256 only.
- **Never in text.** Secrets, sealed values and master keys are never written to a log, a `tracing` field, an error message, an
  audit `details` map or a metric label. Types that hold one implement `Debug` as `"<redacted>"`. Provider and S3 error bodies are
  not copied into messages: the server keeps a status class and a short fixed phrase.
- **Scoped.** A secret belongs to one owner (`OwnerRef`): a user, an organisation or a repository. Resolution order for a run or a
  repository is repository, then organisation, then user, then the instance default. A secret of one owner is never used for another.
- **Tests required:** seal and open round trip, wrong owner or row fails, flipped bit fails, rotation keeps every value readable,
  `Debug` and error output never contain a planted secret (a test plants one and greps all captured logs), file mode check.

## Unit map and order

| # | Unit | Owns (asc) | Owns (hub) | Needs |
|---|---|---|---|---|
| 1 | Policy and audit | `asc-core/src/policy.rs`, `asc-serve/src/policy/`, `asc-serve/src/audit/` | `routers/web/repo/asc_policy.go`, audit page | 0 |
| 2 | Vault and network guard | `asc-serve/src/vault/`, `asc-serve/src/netguard.rs` | admin page for the key | 0 |
| 3 | BYO storage | `asc-serve/src/storage/` | settings page, user/org/repo | 1, 2 |
| 4 | BYO AI keys | `asc-serve/src/aikeys/` | settings page | 1, 2 |
| 5 | Secret scanning, signing | `asc-core/src/secrets.rs`, `asc-serve/src/secretscan/`, `asc-serve/src/signing/` | settings, repo security page | 1 |
| 6 | Stacks (restack, revert, undo) | `asc-serve/src/stacks/` (uses `changes/restack.rs`) | `services/pull/asc_revert_stack.go`, `asc_stack_merge.go`, routes and buttons | 1 |
| 7 | Merge queue | `asc-serve/src/mergequeue/` | `services/automerge/asc_stack_queue.go`, web button | 1, 5, 6 |
| 8 | CI insights | `asc-serve/src/ci/insights/` | checks tab | 1 |
| 9 | Agent tokens and budgets | `asc-serve/src/agents/tokens.rs`, `asc-serve/src/authz/` hooks | token settings | 1, 4 |
| 10 | Agent runs | `asc-serve/src/agents/` (runtime), `asc-mcp` tools | run pages | 4, 9 |
| 11 | Workflows | `asc-serve/src/workflows/`, `asc-core/src/workflow.rs` | workflow pages | 10 |
| 12 | Cross-repo search, symbol panel | `asc-serve/src/codeintel/` (`CrossSearch` service) | explore search, JSON API | 0 |
| 13 | Dependency scanning and update proposals | `asc-serve/src/deps/` (`DepsService`) | security page | 1, 2, 5 |

Numbers 1 and 2 first; 12 is independent. Migration numbers are reserved so parallel units never pick the same file: policy and
audit `0501`-`0503`, vault `0510`, storage `0511`, AI keys `0512`, secret scan `0520`, signing `0521`, stacks `0530`, merge queue
`0531`, CI insights `0540`, agent tokens `0550`, agent runs `0551`, workflows `0552`, dependencies `0560`; each in both `migrations/sqlite` and
`migrations/postgres`. Server config keys go in `crates/asc-serve/src/config.rs` as a section with `deny_unknown_fields` and defaults,
and in `deploy/asc-serve.toml.example`.

---

## 1. Repository policy

*Today:* `Repo.GetPolicy` (read-only) and the parser in `asc-core/src/policy.rs`. *Done when:* the file can be read, validated, proposed
and evaluated through the API and the hub shows it; every key below is parsed strictly and enforced.

- **RPCs** (`policy.proto`, feature `policy-api.v1`): `Policy.GetSource` (repo read), `Validate` (repo read), `Propose` (repo write; makes a
  change on `base`, never writes trunk), `Evaluate` (self: repo read; another principal: maintainer).
- **Data:** the file `.asc/policy.toml` on the trunk tip. No table. The file in force is the one on the tip before the update.
- **File keys added** to the existing schema (all optional, unknown keys are errors): `[secrets] enabled, block_on_push, allow_paths,
  rules.<id>.{pattern,severity,description}` (section 4); `[signing] require_signed` (section 5); `[ci] flaky_threshold, allow_quarantine`
  (section 8); `[agents] enabled, max_runtime_seconds` and `[agents.budgets] max_tokens, max_cost_micro_usd` next to the existing
  `max_changes_per_run`, `max_paths_per_run` (section 11); `[merge] queue = true|false`, `[merge] revalidate = true` (section 7).
- **Config:** none. **Errors:** `POLICY_DENIED`, `POLICY_INVALID`, `POLICY_LOCKOUT` (a file that would leave no maintainer or that the
  proposer could not land is refused with `POLICY_LOCKOUT` by `Validate` as a warning and by `Propose` as an error).
- **Audit:** `policy.proposed`, `policy.denied`.
- **Hub:** a read-only policy page and a "propose change" editor that calls `Propose` and links to the new pull request; the pull request
  of a policy change shows the diff of effective rules.
- **Tests:** strict parse of every new key, bounds, case-insensitive globs, lockout, a change cannot lift the rules that bind it, agent
  caller proposes as agent, `Evaluate` for another principal refused to non-maintainers.

## 2. Network guard (shared by sections 9 and 10)

`asc-serve/src/netguard.rs`, owned by unit 2. One function builds an HTTP client for a user-supplied URL; storage and AI keys use nothing else.

- **Rules.** Scheme must be `https` unless `[netguard] allow_loopback_http = true` (operator opt-in, loopback only). No userinfo,
  fragment or query on a storage endpoint. Port 1-65535 and not in a short deny list (22, 25, 3306, 5432, 6379, 11211, 2375, 2379, 9200)
  unless `[netguard] allow_ports` names it. The host is resolved by the server; **every** resolved address must be public: not loopback,
  unspecified, link-local (169.254/16, fe80::/10), private (10/8, 172.16/12, 192.168/16, fc00::/7), carrier-grade NAT (100.64/10), multicast,
  broadcast, documentation or benchmark ranges, IPv4-mapped or NAT64 forms of those, or a metadata address (169.254.169.254,
  `fd00:ec2::254`, 100.100.100.200, `metadata.google.internal`). The check is by address after resolution, never by name.
- **Pinning.** The connection goes to the checked address (a resolver override in the connector, SNI and the certificate name stay the
  host), so a second lookup cannot return something else (rebinding). Redirects are not followed. Responses are bounded in size and time.
- **Error reasons** (slugs in `ErrorDetail.paths[0]`): `not_https`, `credentials_in_url`, `port`, `unresolvable`, `loopback`, `private`,
  `link_local`, `metadata`, `reserved`.
- **Config:** `[netguard] allow_loopback_http` (default `false`; admits plain `http` only, never `https`, to a loopback address), `loopback_ports` (default `[]`: any port; set, only those ports; needs `allow_loopback_http`), `allow_ports` (default `[]`), `connect_timeout_secs` (10),
  `request_timeout_secs` (30).
- **Tests:** table test over every range above in v4, v6 and mapped forms; a resolver that returns a public then a private address on
  the second call is pinned to the first; a redirect to a private address is not followed; loopback http works only with the opt-in.

## 3. Audit log

*Today:* an `asc audit:` line in the hub log. *Done when:* the append is in every mutating path, the chain verifies, the page filters and exports.

- **RPCs** (`audit.proto`, `audit.v1`): `Audit.List`, `Export`, `VerifyChain`, `ListActions`, `Append` (the host service only; `audit.append.v1`).
- **Append.** The host application keeps settings asc does not see (who may register, the maintenance notice). It records a change of one with `Audit.Append` after the change is made: an action from the catalogue whose name starts with `host.` (`host.setting.updated`, `host.maintenance.updated`), the login of the person as `actor` and the host service as `on_behalf_of`, details cut to the action's allow-list, and the address of the person's request kept as a /24 or /48. Any other caller is `PERMISSION_DENIED`, any other action `INVALID_ARGUMENT`. The event is written to the instance log (and the repository's, when `repo` is set) in one transaction and returns its sequence number. It is not idempotent.
- **Data:** `audit_events(scope_kind, scope_name, seq, id, time_ms, actor_kind, actor_name, on_behalf_kind, on_behalf_name, action, repo, target,
  outcome, request_id, remote_addr, details_json, prev_hash, hash)`; primary key `(scope_kind, scope_name, seq)`. Each event is written
  to every scope it belongs to (repository, owner, instance) in the caller's transaction. `hash = BLAKE3(prev_hash || canonical encoding)`.
  Append-only: the application role has no `UPDATE` or `DELETE` on the table; retention only drops whole sealed segments and writes a
  segment record that keeps the head hash.
- **Details** are built per action from an allow-list of keys (`ListActions` returns them); free text is truncated to 256 bytes; a value
  that matches a secret rule is dropped.
- **Config:** `[audit] retention_days` (default 365; 0 keeps everything), `[audit] export_max_events` (1,000,000), `[audit] undo_window_secs` (600), `[audit] admins`.
  See `docs/AUDIT.md`.
- **Action catalogue** (dotted; units add theirs here): `policy.proposed`, `repo.ref.update`, `repo.ref.undo`, `stack.reverted`,
  `stack.restacked`, `queue.enqueued`, `queue.dequeued`, `queue.landed`, `queue.paused`, `secret.finding.updated`, `signing.key.added`,
  `signing.key.revoked`, `signing.signature.submitted`, `storage.target.created`, `.updated`, `.verified`, `.enabled`, `.deleted`,
  `storage.migration.started`, `.finished`, `ai.key.created`, `.updated`, `.tested`, `.deleted`, `vault.rotated`, `agent.token.created`,
  `.revoked`, `agent.run.created`, `.cancelled`, `.finished`, `workflow.enabled`, `workflow.triggered`, `workflow.approval.decided`.
- **Hub:** `/<owner>/<repo>/settings/asc/audit`, `/org/<org>/settings/asc/audit`, `/-/admin/asc/audit`, with filters and an export button
  (streams `Audit.Export`). Replaces the `asc audit:` log line.
- **Errors:** none of its own. **Tests:** the chain verifies; editing or deleting a row fails `VerifyChain` at that row; `details`
  never holds a planted secret; scope visibility (`NOT_FOUND` for others); export digest matches the bytes.

## 4. Secret scanning

- **RPCs** (`secretscan.proto`, `secretscan.v1`): `SecretScan.ListRules`, `Scan`, `ListFindings`, `UpdateFinding`.
- **Engine:** `asc-core/src/secrets.rs`, pure: built-in detectors (private key blocks, cloud and forge tokens with known prefixes and
  check characters, JWT-shaped values, connection strings with passwords, high-entropy assignments next to key-like names), plus
  custom regex rules from the policy. Linear-time regex only (`regex` crate); inputs are bounded (files larger than 1 MiB and
  binary files are skipped and counted). A finding carries a keyed fingerprint (`HMAC-SHA256(repo key, rule id || value)`) and a
  masked preview; the value is never stored.
- **Where it runs:** before a ref update that lands on a protected branch (in `refpolicy`, after the policy check) over what the update
  adds; with `block_on_push` (default true) a hit refuses the update with `SECRET_DETECTED`. A maintainer-approved bypass is a policy
  `allow_paths` entry, a reviewed change like any other, not a flag on the push.
- **Data:** `secret_findings(id, repo, rule_id, severity, state, path, line, col, changeset, change, fingerprint, preview, first_seen,
  last_seen, resolved_by, resolved_at, note)`, unique on `(repo, fingerprint, path)`.
- **Config:** `[secrets] max_file_bytes` (1 MiB), `max_scan_bytes` (16 MiB), `max_files` (10,000). Policy keys on section 1.
- **Errors:** `SECRET_DETECTED`. **Audit:** `secret.finding.updated`, `secret.push.blocked`, `secret.scan.run`.
- **Hub:** repository security page listing findings; the push refusal text names rule and location only.
- **Tests:** each built-in detector with positive and negative samples, fingerprints stable and not reversible, previews never show more
  than 2 characters, a planted secret never appears in logs or errors, bounded work on adversarial input, `block_on_push` off and on,
  `allow_paths` honoured only from the trunk policy.

## 5. Signed changes

- **RPCs** (`signing.proto`, `signing.v1`): `Signing.ListKeys`, `AddKey`, `RevokeKey`, `SubmitSignature`, `Verify`, `GetServerKey`.
- **Format:** SSHSIG (as `ssh-keygen -Y sign`) in namespace `asc-changeset` over `asc-changeset-v1\n<hex changeset id>\n`; ed25519,
  ecdsa-p256 and rsa 3072+ only. Kept beside the changeset (`change_signatures`), so an id never changes. The client side is
  `asc sign` (crate `asc`, `crates/asc/src/cli/sign.rs`) and the `--sign` flag of `commit`/`modify`, using an ssh-agent or a key file.
- **Data:** `signing_keys(id, owner, title, kind, fingerprint UNIQUE, public_key, created, expires_at, revoked_at, revoked_reason,
  last_used)` and `change_signatures(repo, changeset, signature, key_id, status, signed_at)`.
- **Rules:** `VALID` needs a live, unexpired key of the changeset author; revocation applies to signatures submitted after it, and an
  earlier `VALID` stays explainable. Policy `[signing] require_signed = true` makes landing on trunk need `VALID` for every change; the
  queue revalidates it (section 7). Server-made commits (`Commit`/`Merge` with `sign`) are signed with `[signing] server_key_file`.
- **Config:** `[signing] server_key_file`, `max_keys_per_user` (32). **Errors:** `UNSIGNED_CHANGE`, `SIGNATURE_INVALID`, `SIGNING_KEY_LIMIT`.
- **Audit:** `signing.key.added`, `signing.key.revoked`, `signing.signature.submitted`.
- **Hub:** SSH signing keys under user settings; a "Verified" badge on versions of a change (`Verify`), the reason on hover.
- **Tests:** each key kind valid, wrong namespace, wrong message, key of another user, expired, revoked, option-bearing key lines
  refused, duplicate fingerprint, 100-item verify bound.

## 6. Stacks: restack, revert, undo

*Today:* `Changes.RestackChanges` primitive; hub `TODO(C22)` and `TODO(C52)` markers; `services/pull/asc_revert_stack.go` unrouted.

- **RPCs** (`stacks.proto`, `stacks.v1`): `Stacks.Restack`, `RevertLanded`, `ListRefLog`, `UndoRef`.
- **Behaviour:** `Restack` orders the stack bottom to top, rewrites each change onto its (new) parent with `Changes.RestackChanges`
  semantics, and moves all refs in one atomic update guarded by the expected values; conflicts return with `Resolution`s accepted on the
  next call; frozen changes (in the queue) are `CHANGE_FROZEN`. `RevertLanded` uses `Repo.FindLanding` and `Repo.Apply` (REVERT) from
  the top of the landed stack down onto trunk, and stores the result as new changes under `refs/users/<caller>/`; it never writes
  trunk. `ListRefLog` reads the ref log that the store keeps for `FindLanding` (`ref_log_seq`); `UndoRef` restores the value before an
  entry with a compare-and-swap on the current value, refuses protected refs (trunk, release branches, shared refs) with
  `REF_UNDO_PROTECTED`, and needs a maintainer for another user's `refs/users/` ref.
- **Data:** the ref log already exists; add `ref_log.via` and `op_id` columns if missing (migration `0530`).
- **Errors:** `STACK_NOT_LANDED`, `REVERT_CONFLICT`, `REF_UNDO_PROTECTED`, `REF_LOG_EXPIRED`, plus existing `LAND_CONFLICT`, `CHANGE_FROZEN`.
- **Audit:** `stack.restacked`, `stack.reverted`, `repo.ref.undo`.
- **Hub:** wire the `Stacks` calls into the `AscStackBackend` and `AscRevertBackend` seams, delete the `TODO(C22)` markers, route and
  button for "Revert stack" on a merged stacked pull request and for the server-restack button; the reverts open as a new stacked
  pull request.
- **Tests:** restack of a three-change stack onto a moved trunk (clean, conflicting, partially frozen, atomic failure leaves refs
  untouched), revert of a landed stack (clean, conflict, partial stack, squash), undo (own ref, protected ref refused, moved ref conflicts).

## 7. Merge queue

*Today:* the hub queue lands the largest ready prefix; revalidation after a content merge fails closed. `asc serve` does not mount
the `MergeQueue` service: its methods answer `UNIMPLEMENTED` from the router until it is built, and no client calls it.

- **RPCs** (`mergequeue.proto`, `mergequeue.v1`): `MergeQueue.Enqueue`, `Dequeue`, `List`, `Get`, `Revalidate`, `SetPaused`, `Watch`.
  `Changes.RequestMerge` stays and calls `Enqueue`.
- **Revalidation** (asc side, `mergequeue/revalidate.rs`): for the exact head that would land (after any content merge), check mergeability,
  required checks on that head (`Ci`), approvals of that version (`Review`), `require_signed` (section 5), open secret findings of the
  stack (section 4), and the policy for each path. Any undecidable input keeps the entry out of `READY`. A content merge that changes what
  lands sets `content_merge` and triggers a new revalidation of the merged head before `LANDING`.
- **Data:** `merge_queue(id, repo, target, change, stack_json, state, style, enqueued_by, enqueued_at, revalidation_json, landed_as,
  landed_at, removed_reason)` and `merge_queue_pause(repo, target, paused, reason, by)`.
- **Errors:** `QUEUE_PAUSED`, `QUEUE_ENTRY_EXISTS`, `REVALIDATION_FAILED`, existing `LAND_NEEDS_CONTENT_MERGE`, `CHANGE_FROZEN`.
- **Audit:** `queue.enqueued`, `queue.dequeued`, `queue.landed`, `queue.paused`.
- **Hub:** send `no_content_merge=false` and handle `LAND_NEEDS_CONTENT_MERGE` through `Revalidate`, a
  "Merge when ready" button and queue panel on the pull request page, a queue page per branch (`Watch` through a hub event stream).
- **Tests:** order and prefix landing, pause, revalidation reasons one by one (stale approval, stale checks, signature, secret
  finding, policy), content merge triggers revalidation, concurrent enqueue of the same change, restart recovery of an entry in `LANDING`.

## 8. CI failure grouping and flaky tests

- **RPCs** (`ciinsights.proto`, `ci-insights.v1`): `CiInsights.ReportTestCases`, `ListFailureGroups`, `GetFailureGroup`,
  `SetFailureGroupState`, `FlakyHistory`, `ListFlakyTests`.
- **Grouping:** normalise the first failure line and message (strip numbers, hex ids, uuids, timestamps, absolute paths, ports),
  `signature = BLAKE3(job, test, normalised)`; one group per signature. A test is flaky when it passed and failed on the same head or
  failed then passed on a retry within the window; rate = flips / runs per day bucket. Quarantine only when policy `[ci]
  allow_quarantine = true` and the caller is a maintainer.
- **Data:** `test_cases(run_id, job_id, name, outcome, attempt, duration_ms, group_id, message)`, `failure_groups(...)`, daily rollup
  `flaky_daily(repo, test, job, day, runs, failures, flips)`; messages are masked by the secret rules before storage and truncated to 8 KiB.
- **Config:** `[ci] flaky_threshold` in policy (default 0.02); server `[ci_insights] retention_days` (default 90).
- **Hub:** a "Failures" tab with groups, the flaky list and a per-test history chart; runner reports test cases through `ReportTestCases`
  (a JUnit XML adapter in the hub's Actions bridge, `services/ascsync`).
- **Tests:** normalisation table, grouping across runs, flaky detection on constructed histories, bounds on 5,000 cases, quarantine
  authorisation, replay of the same (run, job) replaces rather than doubles.

## 9. Bring your own storage

- **RPCs** (`storage.proto`, `storage.v1`): `Storage.CreateTarget`, `ListTargets`, `GetTarget`, `UpdateTarget`, `DeleteTarget`,
  `VerifyTarget`, `SetEnabled`, `AssignRepo`, `StartMigration`, `GetMigration`, `ListMigrations`, `CancelMigration`.
- **Lifecycle:** `PENDING` on create or on any change of endpoint, bucket, prefix or credentials; `VerifyTarget` runs `resolve`, `connect`,
  `put`, `get`, `delete` (a random probe object under `<prefix>.asc-probe/`) through the network guard (section 2) and sets `VERIFIED`
  or `FAILED`; only `VERIFIED` can be enabled; a repository can be assigned only an `ENABLED` target of an owner the caller administers.
  Verification is rate limited (6 per target per hour).
- **Migration:** copy every object of the repository to the target, read each back and compare the hash, switch the repository's
  storage pointer in one transaction, then (only if asked) delete the source; reads and writes stay available throughout; a failed or
  cancelled migration leaves the source authoritative. Writes during the copy go to both until the switch.
- **Data:** `storage_targets(id, owner_kind, owner_name, name, endpoint, region, bucket, prefix, addressing, sealed_credentials, key_gen,
  state, last_error, verified_at, created, updated)`, `repo_storage(repo, target_id)`, `storage_migrations(...)`.
- **Config:** `[storage] max_targets_per_owner` (64), `[storage] allow_user_targets` (default `true`); the operator's own S3 setting
  keeps working and is the default target.
- **Errors:** `STORAGE_ENDPOINT_BLOCKED`, `STORAGE_VERIFY_FAILED`, `STORAGE_IN_USE`, `MIGRATION_ACTIVE`, `QUOTA_EXCEEDED`.
- **Hub:** `/user/settings/asc/storage`, `/org/<org>/settings/asc/storage`, repository settings "Storage" picker and migration
  progress; credentials fields are password inputs that are never prefilled.
- **Tests:** every guard rule through the API, verify success and each failing step against a local S3 double (the `garage` compose
  service in CI; loopback http only with the opt-in), credentials never returned or logged, migration happy path, interrupted
  migration, cancel, delete while in use, scoping (another user's target is `NOT_FOUND`).

## 10. Bring your own AI keys

- **RPCs** (`aiproviders.proto`, `ai-keys.v1`): `AiProviders.CreateKey`, `ListKeys`, `GetKey`, `UpdateKey`, `DeleteKey`, `TestKey`,
  `Usage`, `ListModels`.
- **Providers:** Anthropic, OpenAI, Google and OpenAI-compatible endpoints; the client is a small trait in `asc-serve/src/aikeys/provider.rs`
  with one implementation per provider, built on the network guard. A key is `UNTESTED` until `TestKey` succeeds (one minimal request) and
  unusable until `ACTIVE`; a 401 or 403 later marks it `FAILED`. Monthly cap: the sum of recorded cost for the calendar month (UTC)
  stops use with `AI_CAP_REACHED`; the instance cap `[aikeys] instance_monthly_cap_micro_usd` applies on top.
- **Data:** `ai_keys(id, owner_kind, owner_name, label, provider, base_url, default_model, sealed_key, key_gen, state, monthly_cap,
  created, updated, tested_at, last_error)` and `ai_usage(key_id, day, repo, agent, model, requests, input_tokens, output_tokens,
  cost_micro_usd)`.
- **Config:** `[aikeys] max_keys_per_owner` (32), `instance_monthly_cap_micro_usd` (0 = none), price table path `price_file`
  (JSON model to micro-USD per million tokens; unknown model costs are recorded as 0 and flagged in `Usage`).
- **Errors:** `AI_ENDPOINT_BLOCKED`, `AI_KEY_UNTESTED`, `AI_CAP_REACHED`.
- **Hub:** `/user/settings/asc/ai`, organisation and repository equivalents, a usage table; the key field is write-only.
- **Tests:** never returned or logged (planted key), guard on `base_url`, test against a local provider double for each provider (success,
  401, 429, timeout, oversized body), cap enforcement across concurrent calls, owner scoping and resolution order.

## 11. Agent tokens and budgets

- **RPCs** (`agents.proto`, `agent-tokens.v1`): `AgentTokens.CreateToken`, `ListTokens`, `GetToken`, `RevokeToken`, `SetBudget`, `ReportUsage`.
- **Token:** `ascat_` + 43 base64url characters from 32 random bytes; only SHA-256 stored; shown once. Always expires (default 1 hour, at
  most 90 days). Scope = the intersection of the token's `paths`, the policy `[agents] default_scope` (or the rule that names the agent)
  and the policy path rules, minus `denied_paths`; `max_changes_per_run` and the budgets come from the token and the policy, the stricter
  wins. The authz layer (`asc-serve/src/authz/token_store.rs`) resolves it to principal kind `agent` with the scope attached.
- **Enforcement (server side, in the ref-update path and in the provider proxy):** every `UpdateRefs` by an agent principal checks
  scope per path (`AGENT_SCOPE_DENIED` with the refused paths), counts changes and paths against the run and the token
  (`BUDGET_EXCEEDED`), refuses trunk and any non-draft target (an agent writes only to its own user refs, `refs/users/<agent id>/`, SPEC 21.5.1), and charges the
  counters in the same transaction as the ref update. `ReportUsage` adds tokens and cost atomically; crossing a limit is recorded in
  full and answered `BUDGET_EXCEEDED`, and the token becomes `EXHAUSTED`. Expiry and revocation are checked on every call.
- **Data:** `agent_tokens(id, repo, name, agent, secret_sha256 UNIQUE, paths_json, max_changes_per_run, created_by, created, expires_at,
  revoked_at, last_used, limits_json, usage_json)`; usage counters updated with a conditional `UPDATE ... WHERE usage + ? <= limit`.
- **Config:** `[agents] enabled` (false), `max_token_ttl_secs` (7,776,000), `default_limits` (changes 20, tokens 1,000,000, cost 5,000,000
  micro-USD, runtime 3,600 s).
- **Errors:** `AGENTS_DISABLED`, `AGENT_TOKEN_EXPIRED`, `AGENT_TOKEN_REVOKED`, `AGENT_SCOPE_DENIED`, `BUDGET_EXCEEDED`.
- **Audit:** `agent.token.created`, `agent.token.revoked`, `agent.budget.exceeded`.
- **Hub:** repository settings "Agents" page (create, revoke, budgets, usage) behind the agent switch.
- **Tests:** scope intersection table, trunk write refused, budget races (many concurrent updates never exceed), expiry at the second,
  revoke takes effect on the next call, the secret appears once and never again, hash comparison is constant time.

## 12. Agent runs

- **RPCs** (`agents.proto`, `agents.v1`): `Agents.CreateRun`, `ListRuns`, `GetRun`, `CancelRun`, `SendInput`, `WatchRun`.
- **Runtime:** a run is a loop in `asc-serve/src/agents/runtime.rs`: build a workspace view of `base`, call the provider with the owner's key
  (section 10), execute the tools the model asks for, and record each step as an event. Tools are the read side of `asc-mcp` (search,
  symbols, read file, diff) and a write tool that stages file edits; commits are made as the agent principal through the same
  `UpdateRefs` path as anyone (so scope, budget, secret scanning and policy all apply), as a stack in the agent's own user refs.
  There is no shell tool and no network tool. Tool output and file content are untrusted data and are never treated as instructions
  about the budget or scope. The run's own token (section 11) is created per run, expires with it and is revoked when it ends.
- **State machine:** `QUEUED -> RUNNING -> (WAITING_INPUT <-> RUNNING) -> SUCCEEDED | FAILED | CANCELLED | BUDGET_EXCEEDED | EXPIRED`;
  a server restart resumes `RUNNING` runs from their last event or marks them `FAILED` with `restarted`.
- **Data:** `agent_runs(id, repo, agent, title, state, target, base, ai_key_id, model, limits_json, usage_json, changes_json, created_by,
  created, started, finished, failure_code, failure_message, token_id, workflow_run_id, workflow_step)` and `agent_run_events(run_id, seq,
  kind, time_ms, text, tool, change, usage_json)`. Event text is passed through the secret rules before storage.
- **Config:** `[agents] max_concurrent_runs` (4), `max_concurrent_per_repo` (1), `max_task_bytes` (65,536), `event_retention_days` (30).
- **Errors:** `AGENTS_DISABLED`, `RUN_NOT_ACTIVE`, `BUDGET_EXCEEDED`, `AGENT_SCOPE_DENIED`, `AI_KEY_UNTESTED`, `AI_CAP_REACHED`.
- **Audit:** `agent.run.created`, `agent.run.cancelled`, `agent.run.finished`.
- **Hub:** a runs list and a run page with the live event stream (rendered as text), the produced stack linked as pull requests, a cancel
  button, a budget meter; "Ask an agent" entry on a repository (behind the agent switch). Landing is the normal review and queue path.
- **Tests:** a scripted provider double drives a run to a stack; cancel mid-run; budget stop; scope refusal is surfaced to the model as a tool error and
  counted; trunk is never written (also by a hostile tool call); prompt-injection text in a file cannot raise the budget or widen the scope;
  restart recovery; events never hold a planted secret.

- **Run finished (outbox):** when a run ends and has changes, the runtime queues the outbox effect `agent_run_finished` (`agents/runs.rs` `enqueue_finished`). With `[forgejo]` configured, `forgejo::RunFinishedEffect` delivers it, at least once, to `POST /api/internal/asc/agent-run-finished` on the hub with the internal token, `X-Asc-Op-Id` and the body `{repo, run, state, changes}`; the hub opens the changes of a run that succeeded for review. Without a forge the effect is a no-op. The hub answers 503 while a change has no git id yet, which makes the outbox retry.

## 13. Workflows

- **RPCs** (`workflows.proto`, `workflows.v1`): `Workflows.ListDefinitions`, `GetDefinition`, `Validate`, `SetEnabled`, `Trigger`,
  `ListRuns`, `GetRun`, `CancelRun`, `DecideApproval`, `WatchRun`.
- **Definition:** `.asc/workflows/<name>.toml` on the trunk tip, parsed strictly by `asc-core/src/workflow.rs` (unknown keys are errors;
  at most 32 steps, 16 triggers, prompts 16 KiB, file 64 KiB). Keys: `description`, `[[trigger]] kind, branches, paths, cron, agent`,
  `[[input]] name, description, required, default`, `[budget]`, `max_parallel_runs`, `[[step]] id, kind (agent|approval|checks), needs,
  agent, prompt, paths, approvers, checks, timeout_seconds, continue_on_failure, [step.budget]`. A step cannot run commands. The graph
  of `needs` must be acyclic. Templates `{{input.x}}`, `{{change}}`, `{{event.field}}` insert values as text into the prompt, never into
  tool arguments.
- **Execution:** the engine (`workflows/engine.rs`) turns an event into a run when the workflow is enabled, a trigger matches and the
  per-workflow concurrency allows; each agent step creates an agent run (section 12) whose limits are the minimum of the step's, the
  workflow's remaining budget and the policy; approval steps wait for a person admitted by `approvers` (an agent never decides; the
  starter does not approve their own run unless named); checks steps wait on `Ci`. Schedules are evaluated by one elected tick loop with
  at-most-once firing per slot (`workflow_slots`). The version of the definition that runs is the one on the trunk tip at trigger time.
- **Data:** `workflow_state(repo, name, enabled, updated_by)`, `workflow_runs(...)`, `workflow_steps(...)`, `workflow_events(run_id, seq, ...)`,
  `workflow_slots(repo, name, slot)`.
- **Config:** `[workflows] enabled` (false), `max_runs_per_repo_per_day` (200), `min_cron_interval_secs` (900).
- **Errors:** `WORKFLOW_INVALID`, `WORKFLOW_DISABLED`, `AGENTS_DISABLED`, `BUDGET_EXCEEDED`.
- **Audit:** `workflow.enabled`, `workflow.triggered`, `workflow.approval.decided`.
- **Hub:** workflow list, run page with steps and approvals, enable switch, "Run workflow" form from the inputs; events from change opened,
  updated, check failed and review comments mentions are delivered by `services/ascsync` to the asc `Trigger` internal path.
- **Tests:** parse and validate (cycles, unknown keys, bounds), trigger matching, budget sums across steps, approval authorisation, cron
  slots fire once across two ticking instances, a workflow change in an update does not affect the run it triggers, recovery after restart.

## 14. Cross-repository search and symbol panel

*Today:* `asc serve` mounts `asc.v1.CrossSearch` beside `CodeIntel` whenever `[codeintel] enabled` and advertises `cross-search.v1`.
`Search` and `LookupSymbol` run the per-repository `CodeIntel` calls over each repository the caller may read; `SymbolPanel` combines
definition, references and callers of one repository with, on request, references by name in other readable repositories.
A client calls it with the user's own credential.

- **RPCs** (`crosssearch.proto`, `cross-search.v1`): `CrossSearch.Search` (stream), `LookupSymbol`, `SymbolPanel`.
- **Behaviour:** `target.repos` (at most 64) or a `scope` (user, organisation, instance) expands to the repositories the caller may read;
  readability is decided per repository on every call by the same authorisation as `CodeIntel`, and unreadable repositories contribute
  nothing, not even a count. Repositories are searched in name order and share one result and token budget, paged by an opaque token that
  names the repository to resume at and that repository's own token. Patterns shorter than 3 bytes or matching everything are `SEARCH_TOO_BROAD`. The symbol panel
  combines `CodeIntel.Symbols` (def, refs, callers) and, with `cross_repo`, a name lookup in other readable repositories, labelled by
  confidence (`precise`, `import_match`, `name_match`).
- **Data:** none new (uses the code index). **Config:** `[codeintel] cross_search_max_repos` (200 for an instance scope),
  `cross_search_timeout_secs` (20).
- **Errors:** `SEARCH_TOO_BROAD`, `RATE_LIMITED` (rate class `codeintel`).
- **Hub:** route the existing cross-repository handler on the explore page, the symbol panel on the file view, and the JSON API
  `GET /api/v1/asc/search?q=...&mode=...&repo=...` and `GET /api/v1/asc/symbols?name=...` which call this service with the signed-in
  user's token and print the JSON profile. Swagger entries for both.
- **Tests:** readable and unreadable repositories mixed (the unreadable one leaves no trace in counts or errors), private repository
  after a permission change, paging stability, budgets, the JSON output shape, rate limiting.

## 15. Dependency scanning and update proposals

- **RPCs** (`deps.proto`, `deps.v1`): `DepsService.Scan`, `ListFindings`, `ListUpdates`, `ProposeUpdate`, `SetConfig`, `GetConfig`, `Dismiss`.
  Full description: `docs/DEPENDENCIES.md`.
- **Behaviour:** reads the manifests and lockfiles of a revision (no code of the repository runs), normalises them to ecosystem, name,
  version, direct or transitive and path, matches them against OSV (api.osv.dev, a self-hosted mirror or an imported advisory file)
  behind the network guard and an egress allowlist, and asks each public registry for the newest version. A failed lookup is reported
  as unknown, never as clean. `ProposeUpdate` rewrites the pins it can rewrite without a resolver and records a changeset on
  `agent/deps/<group>`; trunk is never written. The repository settings are `.asc/deps.toml`; a schedule is the workflow
  `.asc/workflows/deps.toml` that `GetConfig` renders.
- **Policy:** `[deps] block_severity` (and `block_unknown`, default true) in `.asc/policy.toml` refuses a landing that adds a dependency
  with an advisory at or above that severity, or whose lookup failed, with `POLICY_DENIED`.
- **Data:** `deps_findings`, `deps_cache`, `deps_scans` (migration `0560`).
- **Config:** `[deps]` of `asc-serve.toml`: `enabled` (true), `osv_url`, `advisory_file`, `egress_allow`, the registry base URLs, cache TTLs
  and bounds.
- **Errors:** none of its own (`POLICY_DENIED` from the gate). **Audit:** `deps.finding.updated`, `deps.proposed`, `deps.config.proposed`.
- **Tests:** a parser fixture per ecosystem, an OSV mock server, range evaluation per version scheme, the egress allowlist and the
  network guard, cache TTL, fail-closed gate, proposals never touch trunk, the agent allowlist stays closed to the service.

---

## 16. Calls the host makes for a person (review feature gaps)

- **Claim-aware repository calls.** `Env::context` (`asc-serve/src/review/acting.rs`) is the one place a repository RPC of the `Review`, `Inbox`,
  `Ci` and `Agents` services learns who is calling. The host service (the hub) may attach the signed claim of `asc_rpc::forwarded`
  (`x-asc-forwarded-*`, subject = `owner/name`, role `read`, `write` or `admin`). A claim that verifies and covers the access the RPC needs
  makes the call act as the person (`ctx.principal`) with that role and no more; a claim that does not verify, is for another subject
  (another tenant), is too old or too small is `PERMISSION_DENIED`, never system work; a caller that is not the host ignores the metadata.
  Audit events of such a call name the person as the actor and `host` as `on_behalf_of` (`acting::event`).
- **Hub comments.** `Review.AddComment` takes `external_id` (the hub's own comment id, at most 128 bytes of `[A-Za-z0-9._:/-]`): mirroring
  the same id twice stores it once, and an id never names a comment of another change (`ALREADY_EXISTS`). `ReviewComment.external_id`
  returns it. New audit actions: `review.comment.added`, `review.thread.resolved`, `review.requested`. Migration `0570`.
- **Not built yet** (rows of `docs/asc/feature-gaps/review.md` in the hub): suggested changes and apply-as-vN, the review brief and
  provenance, approval carry-over, agent plan/permission/pause/checkpoint/limit RPCs, spend and monthly budgets, gate statistics and
  overrides, the merge queue server, and the settings/repo/shell rows. See `gaps-review-asc.md`.

## 17. TSRX in code intelligence

- Language `tsrx` (extension `.tsrx`); see SPEC section 23.6. Kinds `component` and `scope` are added to the tag kinds (additive: a
  client that does not know them shows the name). Golden tags: `asc-codeintel/tests/fixtures/tags/sample.tsrx.golden`; scanner tests in
  `asc-codeintel/src/tsrx/tests.rs`.
- `asc stack submit|sync|restack|share` run the same commands as the top-level verbs (they answered "not supported yet").

## 18. Agent control and run boards

- **RPCs** (`agents.proto`): `Agents.ListRequests`, `DecideRequest`, `PauseRun`, `ListCheckpoints`, `RollbackRun`, `SetRunLimits`, `ContinueRun`,
  `GetEstimate`, `RecentTasks`, `GetRunLog`, `ListAgents`, `GetSpend`, `SetMonthlyBudget`. `AgentRun` gains `task`, `progress` (steps done and total,
  current step), `paused`, `pending_requests`, `continues_run`, `for_change`. `CreateAgentRun` gains `plan_first` and `for_change`, and answers
  `lowered` (the limits asked above the policy cap and lowered to it). Migration `0571`.
- **Permission and plan requests** (`asc-serve/src/agents/control.rs`): a command that matches `[agents.sandbox] ask_before`, or the model's
  `request_permission` tool, opens a permission request; a run started with `plan_first` must call `propose_plan` before any other tool and the
  plan is a request too. The run waits in `WAITING_INPUT`; `DecideRequest` allows once / denies a permission or approves / rejects a plan, with the
  decider and a reason stored (the approvals log). Allowing is never a standing grant. A rejected plan goes back to the model with the reason.
- **Pause and rollback.** A paused run holds at its next tool call. Every commit of a run is a checkpoint; a paused run can be rolled back to one:
  the run's ref is forced back to the checkpoint's tip, the working copy is reset, later changes leave the run, and the rollback is audited.
- **Who may.** Reads need repo read. Deciding, pausing, rolling back, editing limits and continuing need repo write and either that the caller
  started the run, a policy maintainer, or repo admin. Monthly budgets need repo admin. A call made by the hub for a person carries that person's
  forwarded claim (section 16) and is audited with the host as relay. A run of another repository is "not found".
- **Spend.** Every model call adds to `agent_spend` (repository, UTC day, agent). `SetMonthlyBudget` caps a repository or one agent per calendar
  month; a run that would start, or a model call that would be made, after the cap is reached is refused or ends with `BUDGET_EXCEEDED` (`monthly`).

## 19. Review brief, approvals, suggestions, gate statistics and overrides

- **RPCs** (`review.proto`): `Review.GetBrief`, `SetBrief`, `RecordApproval`, `CompareVersions`, `PrepareSuggestion`, `MarkSuggestionApplied`,
  `SuggestReviewers`, `RecordGateDecision`, `GetGateStats`, `GrantOverride`, `RevokeOverride`, `ListOverrides`. Migrations `0572`, `0573`.
- **Brief.** Intent (what, why, how, risk, where to look), size and per-file stats from `diffstat.v1`, a deterministic risk level with reasons, the
  provenance (the agent run whose change list holds the change: task, tool calls, files read, write scope) and the data of the gate card (open
  threads, pending and finished reviewers, approvals that apply).
- **Approval carry-over.** The host reports each verdict (`RecordApproval`). A verdict on an older version applies to the newest one when the patch of
  every file that version changes is identical there; otherwise the answer names the files that differ. `CompareVersions` gives changed / unchanged
  files between two versions.
- **Suggestions.** A comment may carry replacement text for its lines (`AddComment.suggestion`). `PrepareSuggestion` returns the file as it would be
  (or says it is stale); the host commits it as the next version and calls `MarkSuggestionApplied`, which records the version. The apply is a normal
  commit, so it is undone with the ordinary ref undo.
  The commit is `Changes.AmendChange` (`changes/restack.rs`): the next version of the change with the same parents, author and message, the newest tree with the named
  files replaced and the newest version as the predecessor. `base_version` must be the newest version and `update_ref` (only `refs/pull/<n>/head`) moves
  only while it still points at it (compare-and-set); otherwise the answer is `stale` and nothing is written. User draft refs never move. The committer
  is the actor; only the forge may name one. Idempotent by `x-asc-op-id`.
- **Gate history.** The host reports each evaluation (`RecordGateDecision`); `GetGateStats` returns checked, passed first time, held, cleared and open,
  the median time to clear, a count per day (zero days included) and per rule. Overrides need repo admin and a reason, are audited with the person
  and the relay, and are chained by a SHA-256 proof.

## 20. Merge queue server

- `MergeQueue` is served by `asc-serve/src/queue/` (`mergequeue.v1`, migration `0574`). Entries are stack prefixes of one target and land in id order.
  `Enqueue` needs repo write, `Dequeue` the owner or admin, `SetPaused` and `ReportEntry` admin, the reads repo read.
- **Fail closed.** `Revalidate` makes an entry READY only when every change has a current approval and none has changes requested; what this
  server cannot decide leaves it WAITING. `ReportEntry` moves an entry to LANDING only from READY and to LANDED only with the landed changeset.
- **Estimates and stream.** `QueueEntry.timing` has `eta_seconds`, `starts_in_seconds`, the stage, the progress and the detail the host reported;
  the median comes from the last 20 landings. `Watch` starts with the live entries, then streams every change from a cursor.

## 21. Notification events for the hub

- `asc-serve/src/notify/` (migration `0576`, table `notify_events`, in `NS_TABLES`). asc serve tells the hub when something happens that a person has
  to know about; the hub decides who may be told and how (inbox, email, quiet hours). The call is `POST /api/internal/asc/event` on the `[forgejo]`
  url with the internal token, body `{event, repo, users, title, detail, path}`, answer `{notified}`.
- **Events.** `agent_question` (a permission, a plan or an `ask_user` question), `agent_finished` (not for a run the person cancelled), `checks_failed`
  (a failed check run of a change, to its authors), `restack_conflict` (to the authors of the change that conflicted), `merge_queue` (landed, blocked or
  dropped, to the person who queued it), `policy_changed` (an update of trunk changed `.asc/policy.toml`, an outbox effect `notify.policy`, to the
  recent contributors), `review_requested` (to the people newly asked) and `mentioned` (an `@login` in a comment, once per comment).
  The hub accepts all eight and tells each `x-asc-event-id` once (the id is `<namespace hex>-<row id>`, which matches `[A-Za-z0-9_.:-]{1,64}`).
- **Limits, enforced here before the hub enforces them.** At most 50 logins per call (a longer list becomes several rows), title 200 bytes, detail
  500 bytes, one line (control characters become spaces), path local to the hub (`/...`, not `//`, no spaces), a repository name `owner/name`.
- **Outbox and retry.** `emit` writes a row and returns; it never fails the operation that caused the event. The dispatcher leases due rows, posts
  them and records the outcome: delivered; refused (a `4xx` except 401, 403, 408, 425 and 429, or an answer it cannot read) is final (401 and 403 are retried: the internal token may be rotating); anything else backs off
  from 1 s to 5 min. A row older than a day is given up; finished rows are deleted after a week. Each post carries `x-asc-event-id`, so a hub can drop a
  redelivery. A key (`dedup`) makes a cause tell once per repository; a repository holds at most 2000 undelivered rows.
- Without `[forgejo]` and its internal token nothing is queued. Every posted delivery and every refusal or give-up is audited (`notify.event.delivered`,
  `notify.event.rejected`).
- **Links.** `[notify]` sets `path_prefix` and the templates `change_path`, `run_path`, `queue_path` and `policy_path` (`{repo}`, `{change}`, `{run}`);
  the defaults are `/{repo}/changes/{change}`, `/{repo}/agents/runs/{run}`, `/{repo}/merge-queue` and `/{repo}/settings/policy`, which the hub rewrites to its own pages.

## 22. Review gaps: conflicts, drafts, secret check, undo preview, babysit

- **Row 81, `Review.GetConflicts`** (repo read, expensive). The conflicts of a change's newest version with a target branch (default branch when empty),
  hunk by hunk: `main` lines, `change` lines, `base` lines when the merge kept them, and the line each side starts at. Read from the marked file the
  mergeability merge stores; a conflict that is not text (binary, modify/delete, file/dir, case) lists only the path and kind. The change stays open and
  nothing is written; `held_back` lists the changes stacked on it. Bounds: 50 files, 20 hunks, 200 lines per side, a 1 MiB marked file.
- **Row 111, `Review.DraftContext`** (repo read, expensive). What "Draft from the diff" is written from, for `base..head` (default branch and HEAD):
  the subjects of the changesets, the files with summed line counts, the size and the risk with its reasons (the brief's classification), and
  `nothing_to_compare`. The patch is read with `Repo.Diff`; the hub writes the text with the person's own key.
- **Row 114, babysit.** `Agents.CreateRun` takes `for_change`: it must be 16 bytes and name a change that has a version in this repository (otherwise
  `NOT_FOUND`), and `AgentRun.for_change` echoes it. The task text is the hub's.
- **Row 119, `SecretScan.Check`** (repo write, expensive, records nothing). The scan of `Scan` for a `rev`, `change` or `range`, for a person about to
  open a change. `blocks` says what the push gate would do under the policy in force: `[secrets] enabled` and `block_on_push`, with a secret found or a scan
  that stopped before it read every file. A finding never carries the value.
- **Row 294, `Stacks.UndoRef` dry run.** The answer lists the changes that leave the ref (`changes_back`, at most 100 in `changes`) and how many of them hold an
  approval of their newest version, which stays (`approvals_kept`). The walk is bounded at 2000 changesets (`preview_truncated`).

## 23. Agent profile privacy

- `Agents.GetPerson` (repo read; cheap) returns the profile a person lets agents read: name, bio, location, website, pronouns. asc keeps no profiles; each call asks
  the hub (`POST /api/internal/asc/agent-profile {login}` with the internal token) and returns `allowed: false` with nothing else when the person does not allow it, does
  not exist, is not public, or the hub cannot be asked (no hub, no token, transport error, any status but 200, an unreadable answer). Errors are not cached; a refusal
  and an answer are kept for 30 and 60 seconds. There is no email and no avatar in the answer, and none is added. Every call is audited (`agent.profile.read`:
  login and whether it was allowed, never the values).

## 24. Insights

`Insights.GetStats` and `Insights.ListContributors` (repo read, expensive) are the data behind the Pulse tiles, the Contributors page and "Who writes the code".
They need no more than repo read and count one repository only. A forwarded claim of the hub is checked as for every repo call.

- **Landed change.** A changeset a land rewrote onto the default branch (`land_map_ref` joined with the `ref_log` entry of that update), counted once per change at
  its first landing in the window. A land that rewrote nothing is not counted.
- **Author.** An *agent* when an agent run's change list holds the change or the first version was published under an `agent:` principal; a *person* otherwise
  (`run_by` is the person who started the run). A person's id is the one recorded with their first version, 0 for a token.
- **Review time.** From the first version of a change to its first verdict (approved or changes requested). `median_review_seconds` is 0 when nothing landed was reviewed.
  `agent_changes_reviewed` counts landed agent changes that hold an approval (only people can approve).
- **Windows.** `days` (1 to 366, default 30) ending at `until` (default now); `landed_previous`, `stacks_opened_previous` and `median_review_previous` cover the window of
  the same length before it. `kind` narrows the tiles, the series, the sizes and the weeks; `landed_by_people` and `landed_by_agents` are always over everyone.
  `per_day` has an entry for every UTC day; `weekly` has the 12 weeks ending at `until`.
- **Sizes.** From stored diff stats only; a landed changeset without one is `unknown`.
- **Stacks opened.** Changes without a parent change that were first published in the window.
- **Bounds.** 50,000 landed rows, 100,000 verdicts, 200 contributors; a cut sets `truncated` and the numbers are lower bounds.

## 25. Policy keys of the Policy board

`Repo.GetPolicy` (and `Policy.Validate`'s `parsed`) now answer every key the form edits, so a form needs no second parse of the file:
`trunk.require_signed`; `agents.enabled`, `max_runtime_seconds`, `max_tokens`, `max_cost_micro_usd` (0 is no limit); `review`; `merge`; `stacks`.

| Key | Default | Meaning |
| --- | --- | --- |
| `[review] author_may_approve` | false | the author of a change may approve it |
| `[review] human_approval` | true | an approval from an agent does not count |
| `[review] keep_approvals_on_restack` | true | an approval carries to a newer version when no file's patch changed; false makes every approval stand for its own version only (enforced when verdicts are listed and by the gate) |
| `[review] dismiss` | admin | who may dismiss another person's verdict on a change: `admin`, `maintainer` (an admin or a member of the `maintainer` role) or `write` (anyone with write access); never an author or agent of the change; a dismissal names the version |
| `[merge] queue`, `revalidate` | false, true | as before |
| `[merge] batch` | 1 | entries the queue tests and lands together, 1 to 20 (read by the queue worker of the host; recorded and returned here) |
| `[merge] post_merge_checks` | false | the checks of the policy run again on the landed tip (recorded and returned; the host runs them) |
| `[stacks] max_depth` | none | the longest stack that may be queued, 1 to 100; a longer one is refused with `QUEUE_STACK_TOO_LONG` |
| `[stacks] cleanup_days` | server setting | days after the last change of a stack lands before its refs are removed, 1 to 3650; the landed-ref reaper (`reftl.rs`) uses it in place of `[cloud] landed_ref_days` for this repository, and a repository that sets it is cleaned even when the server setting is 0 |
| `[stacks] restack_on_main_move` | false | the host restacks open stacks when the default branch moves (recorded and returned; the host acts, the server does not restack by itself because a restack rewrites a person's refs and needs `Stacks.Restack` with that person's right) |
| `[stacks] collaborators_may_push` | false | people with write access who did not start a stack may push onto it (recorded and returned; the host enforces) |

A value out of range, or an unknown key in these sections, makes the file invalid with the key named. `strategy = "squash"` is not part of this change.

## 26. Scheduled backups

`backups.proto`, `crates/asc-serve/src/backups/`, migration 0590. Feature `backups.v1`, configuration `[backups]` (targets by name, a local directory or an S3 bucket;
credentials only as environment variable names; endpoints https). The Admin Backups board: schedule (interval, anchor minute, retention in days then weeks),
history with the result of each run, Back up now, Verify, and a restore that needs the word RESTORE. A run is `asc serve backup`; an S3 target is encrypted in
4 MiB chunks (XChaCha20-Poly1305 under `[backups] key_file`) before upload and keeps a plain sidecar of stored sizes. Restore is staged: the backup must be
verified, the marker `restore.pending.json` is written in `<data dir>/backups`, and the server applies it before opening its database at the next start. Site
administrator only (the host's `instance` claim with role admin, or the host's own credential). Audit: `backup.started`, `backup.finished`, `backup.verified`,
`backup.purged`, `backup.schedule.updated`, `backup.restore.staged`. Runbook: docs/runbooks/backup.md.

## 27. Usage and quotas

`usage.proto`, `crates/asc-serve/src/usage/`, migration 0591. Feature `usage.v1`, configuration `[usage] retention_days`. Four measures for a calendar month: storage
and bandwidth from `usage_counters` (the host reports storage with `Report`; the server adds bandwidth), agent minutes from `agent_runs`, AI spend from
`agent_spend` (organisation, instance) or the runs a person started (person). Spend on the owner's own keys and bring-your-own storage do not count against a quota.
A person reads their own usage, an organisation's owners the organisation's, the site administrator the server's and sets quotas (`usage_quotas`; an owner
without a limit inherits the instance default).

## 28. Organisation settings

`orgsettings.proto`, `crates/asc-serve/src/org/`, migration 0592. Feature `orgsettings.v1`. The Agents card, the default merge policy, organisation agent
identities, and the instance's agent switch and default budget. Resolution order (a later source only tightens): agents allowed = instance AND organisation AND
repository policy; spend ceiling = smallest non-zero of instance default, organisation ceiling and repository budget; a person approves = organisation OR
repository `[review] human_approval`; merge requirements = organisation defaults under the repository's `.asc/policy.toml` (approvals the larger, checks the union,
`allow_force` and `author_may_approve` AND, `require_signed`, `queue`, `revalidate`, `human_approval` OR, an organisation `strategy` wins). `org::policy::apply` is
the function the land gate calls on `LandRequirements`; `OrgHub::agent_gate` is what the agent runtime asks before a run.

## 29. Sessions

`sessions.proto`, `crates/asc-serve/src/sessions/`. Feature `sessions.v1`. Lists and revokes the personal access tokens, SSH keys and agent tokens of a person
(`tok:`, `ssh:`, `agt:` ids); the site administrator may do the same for any account. Another person's id is NOT_FOUND.

## 30. Data export

`dataexport.proto`, `crates/asc-serve/src/export/`, migration 0594. Feature `dataexport.v1`. An asynchronous job producing a JSON Lines archive of the caller's own
data (metadata only, no secret), kept 7 days, streamed with `Download`. One at a time, one per 24 hours. Audit: `export.requested`, `export.finished`,
`export.downloaded`, `export.deleted`.

## 31. AI key sharing and cap rules

`aikeyshares.proto`, `crates/asc-serve/src/ai/share.rs`, migration 0595. Feature `aikeyshares.v1` is advertised with `ai-keys.v1` (the service is mounted with the keys); the host gates its sharing pages on it. A user's or an
organisation's key is shared with a user, organisation or repository; the grantee's runs use it after their own keys (`AiHub::resolve`), never see it, and the
owner sees the use per sharer. `SetKeyCaps` adds the warning percentage (one audit event per month, `ai.key.cap.warning`) and the stop rule (`stop_at_cap`).

## 32. Releases

`Releases` keeps the record of a tagged release; the hub's pages and `asc release` read and write it. asc stores no asset bytes: an asset is a name, a size, a
SHA-256 and the https address the bytes are served from.

- **Notes.** `DraftNotes` lists the changesets in `from..to` (default: the newest published release to the default branch), newest first, at most 500. A change
  is grouped by a `Release-Note: added|changed|fixed|skip` trailer, else by the leading word of its subject (`feat`/`add` is added, `fix`/`bug` is fixed), else
  "changed"; `skip` leaves it out. Each note names its change (when the change index knows the changeset), its author and whether an agent made it. `markdown`
  is the same text as "### Added", "### Changed", "### Fixed".
- **Next.** The newest published release, the number of changesets since it, a proposed tag (`v1.2.3` becomes `v1.2.4`; any other tag gets none) and an existing draft.
- **Create.** Tag 1 to 128 characters of letters, digits and `._+-/`; notes up to 64 KiB; at most 100 assets whose SHA-256 is 64 lowercase hex characters and whose
  address, when given, is https; a tag is taken once (`ALREADY_EXISTS`). The server computes the provenance and keeps it: the jobs of the run that built the release
  (`build_source` and `build_run` name a run reported with `Ci.ReportRun`; passed and total jobs, skipped jobs not counted) and the signature on the release commit
  (the key's owner and fingerprint, and whether it was valid when submitted). `previous_tag` and `changes` say how many changesets are new since the previous release.
- **Drafts.** `draft = true` keeps a release unpublished; only writers list, read or publish it.
- Audit actions: `release.created`, `release.published`.

## 33. Hub integration guide for sections 26 to 31

How the hub calls these services. Every call is made with the hub's service credential (principal `host-service`) and, for a person, the
forwarded claim (`asc-rpc::forwarded`): the person `<id>:<login>`, a role `read`, `write` or `admin`, and the subject the hub's own check
produced. The claim is the whole of the caller's authority; a claim for another subject, a too-small role, a stale or unsigned claim is
`PERMISSION_DENIED` (or `NOT_FOUND` for an id owned by someone else: key ids, session ids, export ids, share ids). Mutating calls carry
`x-asc-op-id` (a UUID); a retry with the same id and the same person gets the first answer. No call returns a secret.

| Subject | Hub check | Claim subject | Role |
| --- | --- | --- | --- |
| a person's own account | the signed-in user | `user:<login>` | read to look, write to change |
| an organisation | owner of the organisation (members read) | `org:<name>` | admin to change, read to look |
| the server | site administrator | `instance` | admin |

Error codes you will meet: `PERMISSION_DENIED` (no right on the subject), `NOT_FOUND` (no such id, or not yours), `INVALID_ARGUMENT`
(a bound or spelling; the message names the field), `FAILED_PRECONDITION` (a run is going, no such backup target, verify before restore,
the export is not ready), `RESOURCE_EXHAUSTED` (one export per 24 hours, 50 shares per key, 100 agents per organisation, a quota),
`ALREADY_EXISTS` (an agent name), `UNAVAILABLE` with "read-only" (a restore is staged: no ref moves until the restart).

### Backups (`asc.v1.Backups`, subject `instance`, role admin)

* Board load: `ListBackups{limit}` gives `backups[]` (state, size, verify, note, progress `done_bytes/total_bytes`, `restorable`), the `schedule`
  (`next_run`, retention) and `status` (`overdue`, `last_success`, `running`, `restore_staged`, `configured`). Poll it every few seconds
  while `status.running`. `ListTargets` fills the target select.
* Back up now / Retry: `RunBackup{target}` (empty target = the schedule's) returns the running row.
* Edit: `SetSchedule{schedule}`; Verify: `VerifyBackup{id}`, then poll `ListBackups` for `verify`.
* Restore dialog: `PreviewRestore{id}` gives `confirmation` ("RESTORE"), `loses_changes_since`, `warnings`; the button sends
  `Restore{id, confirm}` with the typed word. The answer says `restart_required`: the restore is applied when `asc serve` restarts, and
  until then `status.restore_staged` is true and the server refuses ref updates (read-only).
* Restore only works on a backup the server verified and that still matches the record it made of it; any other state is
  `FAILED_PRECONDITION` and the restore row in `ListBackups.status` is not staged.

### Usage (`asc.v1.Usage`)

* Usage and quota page of a person: `GetUsage{owner: USER login}` (claim `user:<login>` read) gives the four meters (`used`, `limit`, `over`),
  `byo_ai_spend_micro_usd` and `byo_storage` for the "BYO does not count" note, `empty`, `over_quota`, `resets_at` and `repos[]` for the table.
  `period` ("YYYY-MM") picks an earlier month. Organisation page: the same with owner ORG and claim `org:<name>` admin.
* Spend and Budget panels: `ListSpend{owner, group_by KEY|USER|REPO|DAY|MONTH, since, until}`; `GetQuota{owner}` for the "of N" text.
* The site administrator reads any account with the `instance` admin claim (written to the audit log as `usage.admin.read`) and sets limits
  with `SetQuota{owner, quota}` (owner INSTANCE sets the default).
* The hub feeds storage: `Report{repo, storage_bytes, byo_storage}` after a push or on a timer, with `x-asc-op-id`. Bandwidth out is counted
  by the server itself (object fetches and git smart-HTTP).

### Organisation settings (`asc.v1.OrgSettings`, subject `org:<name>`)

* Options page: `GetOrgSettings{org}` (read) gives `agents` (allow, a person approves, monthly ceiling), `merge` (the default merge policy,
  `set` says whether any is set), `agent_count`, `instance_allows_agents` and `effective_spend_ceiling_micro_usd`. Save with
  `SetAgentPolicy` / `SetMergeDefaults` (admin). New organisation form: call `SetAgentPolicy` with the "Allow agents" switch.
* Repository policy editor: `ResolveMergePolicy{org, repo, repo_values}` returns `effective` and `tightened[]`, the fields the organisation
  overrides, so the editor can say "your organisation requires 2 approvals". The server enforces the same rule at landing (approvals, checks,
  signature) and at agent run creation (organisation switch, instance switch, monthly ceiling).
* People and Teams pages: `ListAgents{org}`, `CreateAgent`, `UpdateAgent{state: PAUSED}`, `DeleteAgent`. Server settings: `GetInstanceAgents`,
  `SetInstanceAgents` (site administrator).

### Sessions (`asc.v1.Sessions`, subject `user:<login>`)

* Security page: `ListSessions{}` for the signed-in person (claim `user:<login>` read) gives tokens (`tok:`), SSH keys (`ssh:`) and agent tokens
  (`agt:`) with created, last used and expiry. "Sign out" = `RevokeSession{id}` (write); "Sign out everywhere else" =
  `RevokeOtherSessions{keep_id}`. Admin user page: the same calls with `login` set and the `instance` admin claim. The hub's own browser
  sessions stay the hub's.

### Data export (`asc.v1.DataExport`, subject `user:<login>`)

* "Export your data": `RequestExport` (write) returns a job; poll `GetExport{id}` until `state` is READY (or FAILED with `error`). Mail the
  person a link to a hub route that calls `Download{id}` (server stream of `ExportChunk{offset,data,last}`) and relays the bytes. The archive is
  deleted after `expires` (7 days); `DeleteExport` removes it earlier. A second request within 24 hours is `RESOURCE_EXHAUSTED`.

### AI keys (`asc.v1.AiKeyShares` beside `AiProviders`)

* Key page of a person or organisation (claim `user:<login>` write, or `org:<name>` admin): `ShareKey{key_id, grantee USER|ORG|REPO,
  allow_use, see_spend}`, `ListShares{key_id}`, `RevokeShare`, `ShareUsage{key_id}` (use per sharer, the owner's own use last),
  `GetKeyCaps` / `SetKeyCaps{warn_percent, stop_at_cap}` (the monthly cap itself is `AiProviders.UpdateKey`). `stop_at_cap` off means the
  key's own cap only warns; the instance cap always stops. Without a caps row the cap is a hard stop (`AI_CAP_REACHED`).
* Grantee side: `ListSharedWithMe{grantee}` (organisation admin, repository admin or the person) shows label, provider, `usable` and, when the
  owner allows `see_spend`, the grantee's own spend this month. Never the key.
* Resolution order of a run: repository keys and keys shared with it, the organisation's and those shared with it, the starter's and those shared with
  them, then the instance's.

## Error codes

All are `ErrorDetail.code` values, registered in `crates/asc-rpc/src/errors.rs` and mapped in `server.rs`; the test
`buildout_codes_map_to_their_grpc_status` pins the table.

| Code | gRPC status | Raised by |
|---|---|---|
| `POLICY_DENIED` | PERMISSION_DENIED | ref policy (existing use) |
| `POLICY_INVALID` | INVALID_ARGUMENT | `Policy.Propose` |
| `POLICY_LOCKOUT` | FAILED_PRECONDITION | `Policy.Propose` |
| `SECRET_DETECTED` | FAILED_PRECONDITION | `UpdateRefs`, `Commit` |
| `UNSIGNED_CHANGE` | FAILED_PRECONDITION | land, queue |
| `SIGNATURE_INVALID` | INVALID_ARGUMENT | `Signing.SubmitSignature` |
| `SIGNING_KEY_LIMIT` | RESOURCE_EXHAUSTED | `Signing.AddKey` |
| `STACK_NOT_LANDED` | FAILED_PRECONDITION | `Stacks.RevertLanded` |
| `REVERT_CONFLICT` | ABORTED | `Stacks.RevertLanded` |
| `REF_UNDO_PROTECTED` | PERMISSION_DENIED | `Stacks.UndoRef` |
| `REF_LOG_EXPIRED` | OUT_OF_RANGE | `Stacks.UndoRef` |
| `QUEUE_PAUSED` | FAILED_PRECONDITION | `MergeQueue.Enqueue` |
| `QUEUE_ENTRY_EXISTS` | ALREADY_EXISTS | `MergeQueue.Enqueue` |
| `REVALIDATION_FAILED` | FAILED_PRECONDITION | `MergeQueue.Revalidate` |
| `STORAGE_ENDPOINT_BLOCKED` | INVALID_ARGUMENT | `Storage.*` |
| `STORAGE_VERIFY_FAILED` | FAILED_PRECONDITION | `Storage.SetEnabled` |
| `STORAGE_IN_USE` | FAILED_PRECONDITION | `Storage.DeleteTarget` |
| `MIGRATION_ACTIVE` | ABORTED | `Storage.StartMigration` |
| `VAULT_LOCKED` | UNAVAILABLE | every call that seals or opens a secret |
| `ROTATION_ACTIVE` | ABORTED | `Vault.Rotate` |
| `AI_ENDPOINT_BLOCKED` | INVALID_ARGUMENT | `AiProviders.*` |
| `AI_KEY_UNTESTED` | FAILED_PRECONDITION | agent runs |
| `AI_CAP_REACHED` | RESOURCE_EXHAUSTED | provider calls |
| `AGENTS_DISABLED` | FAILED_PRECONDITION | `Agents.*`, `AgentTokens.*`, `Workflows.Trigger` |
| `AGENT_TOKEN_EXPIRED` | UNAUTHENTICATED | authz |
| `AGENT_TOKEN_REVOKED` | UNAUTHENTICATED | authz |
| `AGENT_SCOPE_DENIED` | PERMISSION_DENIED | `UpdateRefs` by an agent |
| `BUDGET_EXCEEDED` | RESOURCE_EXHAUSTED | `UpdateRefs`, `ReportUsage`, runs |
| `RUN_NOT_ACTIVE` | FAILED_PRECONDITION | `Agents.SendInput`, `CancelRun` |
| `WORKFLOW_INVALID` | INVALID_ARGUMENT | `Workflows.Validate` |
| `WORKFLOW_DISABLED` | FAILED_PRECONDITION | `Workflows.Trigger` |
| `SEARCH_TOO_BROAD` | INVALID_ARGUMENT | `CrossSearch.*` |

## Server configuration keys

Sections of `asc-serve.toml` introduced above, each with `deny_unknown_fields` and defaults that are safe when absent: `[vault]`
(`key_file`, default next to the state directory; created on first start), `[netguard]`, `[storage]`, `[aikeys]`, `[agents]`, `[workflows]`,
`[signing]`, `[secrets]`, `[deps]`, `[audit]`, `[ci_insights]`, and the `[codeintel]` additions. `deploy/asc-serve.toml.example` documents each key.
Hub keys (app.ini `[asc]`): none required; the hub reads the features from `Repo.Version` and shows a surface only when its feature
string is present.

<a id="rpc-index"></a>
## RPC index

Every method of the build-out services (98). A test keeps this list equal to the protos.

<!-- rpc-index:begin -->
**Policy** (`policy.proto`)

- `asc.v1.Policy/GetSource` — repo read
- `asc.v1.Policy/Validate` — repo read; no state is kept
- `asc.v1.Policy/Propose` — repo write; mutating
- `asc.v1.Policy/Evaluate` — repo read for the caller; maintainer for another principal

**SecretScan** (`secretscan.proto`)

- `asc.v1.SecretScan/ListRules` — repo read
- `asc.v1.SecretScan/Scan` — repo admin; expensive; records findings
- `asc.v1.SecretScan/Check` — repo write; expensive; records nothing; says whether the push gate would refuse
- `asc.v1.SecretScan/ListFindings` — repo admin; paged
- `asc.v1.SecretScan/UpdateFinding` — repo admin; mutating

**Signing** (`signing.proto`)

- `asc.v1.Signing/ListKeys` — own keys; another user's needs a site administrator
- `asc.v1.Signing/AddKey` — user identity; mutating
- `asc.v1.Signing/RevokeKey` — owner or site administrator; idempotent
- `asc.v1.Signing/SubmitSignature` — repo write; mutating
- `asc.v1.Signing/Verify` — repo read; cheap
- `asc.v1.Signing/GetServerKey` — any caller; the key that signs server-made commits

**Audit** (`audit.proto`)

- `asc.v1.Audit/List` — cheap, paged
- `asc.v1.Audit/Export` — expensive; rate class `export`
- `asc.v1.Audit/VerifyChain` — expensive
- `asc.v1.Audit/ListActions` — cheap; the catalogue for filters

**Stacks** (`stacks.proto`)

- `asc.v1.Stacks/Restack` — repo write; mutating
- `asc.v1.Stacks/RevertLanded` — repo write; mutating
- `asc.v1.Stacks/ListRefLog` — repo read; cheap, paged
- `asc.v1.Stacks/UndoRef` — repo write (own refs) or maintainer; mutating

**MergeQueue** (`mergequeue.proto`)

- `asc.v1.MergeQueue/Enqueue` — repo write; mutating
- `asc.v1.MergeQueue/Dequeue` — entry owner or maintainer; idempotent
- `asc.v1.MergeQueue/List` — repo read; cheap, paged
- `asc.v1.MergeQueue/Get` — repo read
- `asc.v1.MergeQueue/Revalidate` — repo write; may be slow
- `asc.v1.MergeQueue/SetPaused` — maintainer; idempotent
- `asc.v1.MergeQueue/Watch` — repo read; long-lived, resumable
- `asc.v1.MergeQueue/Preview` — repo read
- `asc.v1.MergeQueue/ReportEntry` — repo admin; the host reports landing; fail closed

**CiInsights** (`ciinsights.proto`)

- `asc.v1.CiInsights/ReportTestCases` — repo admin (service credential); idempotent per (run, job)
- `asc.v1.CiInsights/ListFailureGroups` — repo read; paged
- `asc.v1.CiInsights/GetFailureGroup` — repo read; paged occurrences
- `asc.v1.CiInsights/SetFailureGroupState` — repo write; mutating
- `asc.v1.CiInsights/FlakyHistory` — repo read
- `asc.v1.CiInsights/ListFlakyTests` — repo read; paged

**Storage** (`storage.proto`)

- `asc.v1.Storage/CreateTarget` — mutating, x-asc-op-id
- `asc.v1.Storage/ListTargets` — owner admin; cheap
- `asc.v1.Storage/GetTarget` — owner admin
- `asc.v1.Storage/UpdateTarget` — owner admin; mutating
- `asc.v1.Storage/DeleteTarget` — owner admin; refused while in use
- `asc.v1.Storage/VerifyTarget` — owner admin; probe object round trip; rate limited
- `asc.v1.Storage/SetEnabled` — owner admin; VERIFIED only
- `asc.v1.Storage/AssignRepo` — repo admin and target owner admin
- `asc.v1.Storage/StartMigration` — repo admin; mutating
- `asc.v1.Storage/GetMigration`
- `asc.v1.Storage/ListMigrations`
- `asc.v1.Storage/CancelMigration` — idempotent

**Vault** (`vault.proto`)

- `asc.v1.Vault/Status` — site administrator; cheap
- `asc.v1.Vault/Rotate` — site administrator; mutating
- `asc.v1.Vault/GetRotation`

**AiProviders** (`aiproviders.proto`)

- `asc.v1.AiProviders/CreateKey` — owner admin; mutating
- `asc.v1.AiProviders/ListKeys` — owner admin
- `asc.v1.AiProviders/GetKey` — owner admin
- `asc.v1.AiProviders/UpdateKey` — owner admin; mutating
- `asc.v1.AiProviders/DeleteKey` — owner admin; refused while a live run or token uses it
- `asc.v1.AiProviders/TestKey` — owner admin; one tiny request to the provider; rate limited
- `asc.v1.AiProviders/Usage` — owner admin
- `asc.v1.AiProviders/ListModels` — owner admin; the models a tested key can use

**Agents** (`agents.proto`)

- `asc.v1.Agents/CreateRun` — repo write; mutating
- `asc.v1.Agents/ListRuns` — repo read; cheap, paged
- `asc.v1.Agents/GetRun` — repo read
- `asc.v1.Agents/CancelRun` — run owner or maintainer; idempotent
- `asc.v1.Agents/SendInput` — run owner; mutating
- `asc.v1.Agents/WatchRun` — repo read; long-lived, resumable
- `asc.v1.Agents/ListRequests` — repo read; paged; the approvals log
- `asc.v1.Agents/DecideRequest` — run starter, maintainer or admin; allow once / deny / approve / reject; mutating
- `asc.v1.Agents/PauseRun` — run starter, maintainer or admin; idempotent
- `asc.v1.Agents/ListCheckpoints` — repo read
- `asc.v1.Agents/RollbackRun` — run starter, maintainer or admin; the run must be paused; mutating
- `asc.v1.Agents/SetRunLimits` — run starter, maintainer or admin; never above the policy cap
- `asc.v1.Agents/ContinueRun` — run starter, maintainer or admin; mutating
- `asc.v1.Agents/GetEstimate` — repo read
- `asc.v1.Agents/RecentTasks` — repo read; own tasks, admin sees all
- `asc.v1.Agents/GetRunLog` — repo read
- `asc.v1.Agents/ListAgents` — repo read
- `asc.v1.Agents/GetSpend` — repo read
- `asc.v1.Agents/SetMonthlyBudget` — repo admin; mutating
- `asc.v1.Agents/GetPerson` — repo read; the profile a person lets agents read, asked of the hub each time; fails closed

**AgentTokens** (`agents.proto`)

- `asc.v1.AgentTokens/CreateToken` — repo admin; mutating
- `asc.v1.AgentTokens/ListTokens` — repo admin
- `asc.v1.AgentTokens/GetToken` — repo admin, or the token itself
- `asc.v1.AgentTokens/RevokeToken` — repo admin; idempotent
- `asc.v1.AgentTokens/SetBudget` — repo admin; may raise or lower
- `asc.v1.AgentTokens/ReportUsage` — the agent token itself; mutating

**Workflows** (`workflows.proto`)

- `asc.v1.Workflows/ListDefinitions` — repo read
- `asc.v1.Workflows/GetDefinition` — repo read
- `asc.v1.Workflows/Validate` — repo read; no state is kept
- `asc.v1.Workflows/SetEnabled` — maintainer; idempotent
- `asc.v1.Workflows/Trigger` — repo write; mutating
- `asc.v1.Workflows/ListRuns` — repo read; paged
- `asc.v1.Workflows/GetRun` — repo read
- `asc.v1.Workflows/CancelRun` — run starter or maintainer; idempotent
- `asc.v1.Workflows/DecideApproval` — named approver; mutating
- `asc.v1.Workflows/WatchRun` — repo read; long-lived, resumable
- `asc.v1.Workflows/ListSecrets` — repo admin; names and hints only
- `asc.v1.Workflows/SetSecret` — repo admin; mutating
- `asc.v1.Workflows/DeleteSecret` — repo admin; idempotent

**CrossSearch** (`crosssearch.proto`)

- `asc.v1.CrossSearch/Search` — read on each repository searched; expensive; rate class `codeintel`
- `asc.v1.CrossSearch/LookupSymbol` — read on each repository; cheap (index)
- `asc.v1.CrossSearch/SymbolPanel` — repo read, plus read on repositories that reference it

**DepsService** (`deps.proto`)

- `asc.v1.DepsService/Scan` — repo maintainer; expensive; records findings
- `asc.v1.DepsService/ListFindings` — repo read; paged
- `asc.v1.DepsService/ListUpdates` — repo read; calls the registries
- `asc.v1.DepsService/ProposeUpdate` — repo maintainer; mutating; never trunk
- `asc.v1.Insights/GetStats` — repo read; expensive; landed, stacks opened, review time, author mix
- `asc.v1.Insights/ListContributors` — repo read; expensive; landed and reviews per person and agent
- `asc.v1.Releases/DraftNotes` — repo read; expensive; the changes since a release, grouped
- `asc.v1.Releases/Next` — repo read; changes since the latest release and a proposed tag
- `asc.v1.Releases/Create` — repo write; mutating; a draft or a published release
- `asc.v1.Releases/Publish` — repo write; mutating
- `asc.v1.Releases/List` — repo read (drafts: repo write); paged
- `asc.v1.Releases/Get` — repo read (a draft: repo write)
- `asc.v1.DepsService/SetConfig` — repo maintainer; mutating; never trunk
- `asc.v1.DepsService/GetConfig` — repo read
- `asc.v1.DepsService/Dismiss` — repo maintainer; mutating
- `asc.v1.Backups/ListBackups` — site admin
- `asc.v1.Backups/ListTargets` — site admin
- `asc.v1.Backups/RunBackup` — site admin
- `asc.v1.Backups/SetSchedule` — site admin
- `asc.v1.Backups/VerifyBackup` — site admin
- `asc.v1.Backups/PreviewRestore` — site admin
- `asc.v1.Backups/Restore` — site admin
- `asc.v1.Usage/GetUsage` — owner: the four measures and the repository table
- `asc.v1.Usage/ListSpend` — owner: AI spend grouped by key, user, repository, day or month
- `asc.v1.Usage/GetQuota` — owner: the limits that apply
- `asc.v1.Usage/SetQuota` — site admin
- `asc.v1.Usage/Report` — host service or site admin
- `asc.v1.OrgSettings/GetOrgSettings` — org read
- `asc.v1.OrgSettings/SetAgentPolicy` — org admin
- `asc.v1.OrgSettings/SetMergeDefaults` — org admin
- `asc.v1.OrgSettings/ResolveMergePolicy` — org read
- `asc.v1.OrgSettings/ListAgents` — org read
- `asc.v1.OrgSettings/CreateAgent` — org admin
- `asc.v1.OrgSettings/UpdateAgent` — org admin
- `asc.v1.OrgSettings/DeleteAgent` — org admin
- `asc.v1.OrgSettings/GetInstanceAgents` — site admin
- `asc.v1.OrgSettings/SetInstanceAgents` — site admin
- `asc.v1.Sessions/ListSessions`
- `asc.v1.Sessions/RevokeSession` — mutating
- `asc.v1.Sessions/RevokeOtherSessions` — mutating
- `asc.v1.DataExport/RequestExport` — mutating
- `asc.v1.DataExport/ListExports`
- `asc.v1.DataExport/GetExport`
- `asc.v1.DataExport/Download` — a ready archive, in chunks of at most 1 MiB
- `asc.v1.DataExport/DeleteExport` — mutating
- `asc.v1.AiKeyShares/ShareKey` — key owner
- `asc.v1.AiKeyShares/ListShares` — key owner
- `asc.v1.AiKeyShares/RevokeShare` — key owner
- `asc.v1.AiKeyShares/ShareUsage` — key owner: use per sharer
- `asc.v1.AiKeyShares/ListSharedWithMe` — grantee administrators
- `asc.v1.AiKeyShares/GetKeyCaps` — key owner
- `asc.v1.AiKeyShares/SetKeyCaps` — key owner
<!-- rpc-index:end -->
