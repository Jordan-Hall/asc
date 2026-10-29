# Audit log and ref undo

Every change that matters for security or for trust is written to an append-only, hash-chained log in the same
transaction as the change itself. If the log cannot be written, the change does not happen. This page says what is
logged, who can read it, how to export it, how retention works and how to put a ref back.

Feature strings: `audit.v1` (log, export, verification) and `stacks.v1` (`ListRefLog`, `UndoRef`). Wire contract:
`proto/asc/v1/audit.proto` and `stacks.proto`. The contract for the whole build-out is `docs/BUILDOUT.md`, sections 3 and 6.

## What is logged

One event holds: who (kind and name of the actor, and the person behind an agent token), what (a dotted action from
a closed catalogue, `Audit.ListActions`), when (milliseconds), where from (the network prefix of the client and its user
agent; an IPv4 address is cut to /24 and an IPv6 address to /48), the repository and the target, the outcome (`success`,
`denied`, `failed`), the request id and a small `details` map.

| Area | Actions |
|---|---|
| Refs and pushes | `repo.ref.update` (one per ref, old and new value), `repo.ref.undo`, `push.refused` (a push refused by authorisation, policy, a hook, a quota or a secret) |
| Repositories | `repo.created`, `repo.renamed`, `repo.default_branch`, `repo.deleted` |
| Policy and security | `policy.proposed`, `policy.denied`, `secret.finding.updated`, `secret.push.blocked`, `signing.*` |
| Credentials | `auth.token.created`, `auth.token.revoked`, `auth.ssh_key.added`, `auth.ssh_key.removed`, `agent.token.*`, `ai.key.*`, `storage.target.*`, `vault.rotated` |
| CI | `ci.failure_group.state` |
| The log itself | `audit.exported`, `audit.retention.pruned` |

Tokens and SSH keys are minted by the operator tools (`asc serve token`, `asc serve ssh-key`), so their actor is
`system:operator`; the event names the token's user and repository, which makes it visible in their scopes.

**Details never hold a secret.** Each action has a list of detail keys (`Audit.ListActions` returns them); any other key is
dropped, text is cut to 256 bytes, a URL is reduced to scheme, host and path, and a value that looks like a credential (a
provider token, a private key, a JWT, `password=...`, a long base64 run) is dropped. A token is logged by its public id,
an SSH key by its fingerprint, a storage target by its name.

## Scopes and who may read them

An event is written to every scope it belongs to: its repository, its owner and the instance. Each scope has its own
gap-free sequence and its own chain.

| Scope | Reader |
|---|---|
| a repository | admin on the repository |
| a user | that user |
| an organisation | an owner of every repository of the organisation |
| the instance | a site administrator: the host application's service credential, or a user named in `[audit] admins` |

A caller who may not read a scope gets `NOT_FOUND`, so the answer does not say whether it exists.

## Reading, filtering and exporting

* `Audit.List`: newest first, paged, filtered by actors (`name` or `kind:name`), actions (exact or a prefix ending in `.*`),
  outcome, target prefix and time range.
* `Audit.Export`: the same filters, streamed as JSON lines (one event per line) or CSV, in chunks of at most 1 MiB cut at a
  row end. The last message carries the BLAKE3 digest of the whole export and the number of events. An export holds at most
  `[audit] export_max_events` events; if more matched, `truncated` is set. An export is itself logged (`audit.exported`).
* `Audit.VerifyChain`: recomputes every hash of a scope between two sequence numbers. It finds an edited event (at that
  event), a removed event (at the gap) and removed newest events (the head no longer matches).

## Retention

`[audit] retention_days` (default 365; 0 keeps everything). A worker drops events older than that, oldest first, per scope.
Before it drops anything it writes a segment record with the hash of the last dropped event, so the remaining events still
verify and the drop cannot be mistaken for tampering. A scope whose chain is already broken is left alone: retention never
hides evidence. Each pass is itself logged (`audit.retention.pruned`).

```toml
[audit]
retention_days = 365       # 0 keeps everything
export_max_events = 1000000
prune_interval_secs = 3600
undo_window_secs = 600     # protected-ref undo; 0 switches it off
admins = ["alice"]         # users who may read the instance scope
```

## Undo

`Stacks.ListRefLog` lists the ref log (every ref change the server made, newest first). Each entry says whether it can be
undone and, if not, why. `Stacks.UndoRef` puts a ref back to the value it had before an entry.

Four guards hold for every ref:

1. **Your own operation.** The entry's actor is the caller. Nobody undoes another person's push, a maintainer included.
2. **Only while the ref still matches.** The entry must be the newest of its ref and the ref must still hold the value the
   entry wrote. If anything moved it since, the answer is `VERSION_CONFLICT` and nothing changes. The check is repeated
   with a compare-and-swap inside the transaction that writes, so two undos at once cannot both win.
3. **The old value still exists.** A garbage-collected changeset cannot come back (`FAILED_PRECONDITION`). An entry older
   than the log is `REF_LOG_EXPIRED`.
4. **Retries are safe.** A retried `x-asc-op-id` returns the first answer and writes nothing twice.

Your own branches (`refs/users/<you>/...`) can be undone at any time. A **protected ref** (the trunk, a release branch, a tag,
a shared ref) can be undone only by a maintainer, only for their own update and only within `[audit] undo_window_secs` of
it: it is the roll-back of a land you just did. Otherwise the answer is `REF_UNDO_PROTECTED`, and the way back is
`Stacks.RevertLanded`, which makes new changes and never rewrites history.

Undoing a land also removes what the land recorded besides the ref (the landing map, the mainline index above the restored
tip and the landed marker of the actor's draft branches), so the changes show as not landed again.

The undo is itself a ref update: it has a ref log entry (`via = "undo"`), an audit event `repo.ref.undo` with the undone
sequence number and the reason, and an outbox row for the forge adapter. It can be undone in turn by the same rules. A
refusal for lack of the right is logged as a denied `repo.ref.undo`. `dry_run` checks everything and changes nothing.
