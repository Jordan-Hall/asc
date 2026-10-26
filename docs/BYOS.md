# Bring your own storage

A user, an organisation or a single repository can keep its objects in an S3-compatible bucket it owns. The server stays the place
that knows the repository (refs, changes, review, search); the bucket holds the content-addressed objects. This page says how to use
it, how it works, and what it defends against.

Feature string: `storage.v1` (targets, assignment, migration) and `vault.v1` (the master key). Wire contract: `proto/asc/v1/storage.proto`
and `vault.proto`; the contract for the whole build-out is `docs/BUILDOUT.md`, section 9.

## Using it

1. **Create a target** (`Storage.CreateTarget`): owner, a name (`a-z0-9-`), the endpoint (`https://...`), region, bucket, an optional
   prefix, addressing (auto, path or virtual-host) and an access key pair (plus a session token for temporary credentials). The target
   starts `PENDING`. The key pair is accepted here and never returned: a target shows `access_key_hint` (`AKIA...WXYZ`) and `has_secret`.
2. **Verify it** (`VerifyTarget`): the server resolves the endpoint, connects to the checked address, writes a random probe object under
   `<prefix>/.asc-probe/`, reads it back, compares and deletes it. Each step is reported (`resolve`, `connect`, `put`, `get`, `delete`)
   with a fixed phrase on failure. A target that passes is `VERIFIED`; one that fails is `FAILED` with `last_error`. Six verifications
   per target per hour.
3. **Enable it** (`SetEnabled`): only a verified target can be enabled.
4. **Use it** for a repository, in one of two ways:
   * `AssignRepo`: new objects go to the target from now on. Objects stored before stay where they are and stay readable (the earlier
     place becomes the *fallback*). A repository can hold objects in two places this way, never three: assign again only after a
     migration.
   * `StartMigration`: move everything (below). `to_target_id` empty moves the repository back to the instance bucket.

Changing the endpoint, region, bucket, prefix, addressing or credentials of a target sends it back to `PENDING` and stops its use until
it is verified and enabled again. A target cannot be deleted or disabled while a repository keeps objects in it or a migration involves it
(`STORAGE_IN_USE`).

### Who may do what

| Call | Allowed |
|---|---|
| targets of a user | that user, signed in as themselves (not a deploy key or CI task) |
| targets of a repository | anyone with admin on the repository |
| targets of an organisation | the host application, which checks its own permissions first and then calls with its service credential |
| `AssignRepo`, `StartMigration`, `CancelMigration`, migration reads | admin on the repository, and for a target, admin of the target's owner |
| `Vault.*` | site administrator (the host application's service credential) |

A call that names an owner and is not allowed answers `PERMISSION_DENIED`. A call that names an id (a target, a migration) and is not
allowed answers `NOT_FOUND`, the same as for an id that does not exist.

### Configuration

```toml
[vault]
key_file = "/var/lib/asc/vault.keys"     # default: next to the SQLite database, else in cache.disk_dir

[netguard]
allow_loopback_http = false              # operator opt-in for a local S3 in development
allow_ports = []                         # ports allowed besides the usual ones (the deny list: 22 25 3306 5432 6379 11211 2375 2379 9200)
connect_timeout_secs = 10
request_timeout_secs = 30

[storage]
max_targets_per_owner = 64
max_repos_per_target = 1000
allow_user_targets = true                # false: only organisations and repositories may add targets
health_interval_secs = 300               # 0 turns the checks off
repin_secs = 300                         # how often a target's DNS answer is judged again
migration_settle_secs = 5                # how long replicas get to notice a route change
migration_workers = 2
```

The operator's own bucket (`store`) keeps working and is the default for every repository without a target.

## The vault

Credentials are sealed with XChaCha20-Poly1305 before they reach the database. The data key is HKDF-SHA256 over the master key with the
context `"asc-vault-v1" || kind || owner || row id`, and the same context is the associated data, so a sealed value cannot be moved to
another owner or row. Stored form: `generation (u32) || nonce (24) || ciphertext || tag`.

* **Master key file.** 32 random bytes per generation in `[vault] key_file`, created on first start with mode `0600` (on Windows, a
  protected ACL for the current user and SYSTEM) and refused on load when anyone else could read it, when it is a link, or when it does not
  parse. The vault then stays locked, the server keeps running, and every call that seals or opens a secret answers `VAULT_LOCKED`
  with the reason. Fix the file; the vault unlocks by itself within seconds.
* **Several replicas** share one key file (mount it). A replica notices a changed file and reloads it; a value sealed under a generation
  a replica does not hold makes it re-read the file once.
* **Rotation** (`Vault.Rotate`): writes a new generation first (a failure changes nothing), makes it active and re-seals every value in
  the background in batches, with progress in `Vault.GetRotation`. A rotation that a stopped server left unfinished is resumed at the next
  start. With `retire_old`, generations nothing is sealed under are dropped from the file at the end. Back the file up before rotating.
* **Other features** (AI keys) seal through the same `Vault::seal` and `Vault::open` and register a `Resealer` so a rotation reaches
  their rows.
* A secret is never in a log line, a `tracing` field, an error message, an audit event or a metric. Types that hold one print
  `<redacted>`. The provider's error text is never copied: it is read to choose one of a few fixed phrases.

## The network guard

The server connects to an address a user typed. `netguard.rs` is the one place that does it.

1. **Syntax.** `https` only (plain `http` only under `allow_loopback_http`, and then only to loopback). No user name, password, query or
   fragment. A port from 1 to 65535 that is not on the deny list. Host names of cloud metadata services are refused before any lookup.
   IP spellings (`2130706433`, `0x7f.1`, octal, IPv4-mapped) are normalised by the URL parser before the check.
2. **Resolution.** The server resolves the host itself and judges **every** address, by address and never by name: loopback, unspecified,
   link-local, private (RFC 1918, unique-local, carrier-grade NAT), multicast, broadcast, documentation, benchmark and other reserved
   ranges, the IPv4-mapped, NAT64 and 6to4 forms of those, and the metadata addresses (`169.254.169.254`, `fd00:ec2::254`,
   `100.100.100.200`). One bad address refuses the endpoint. For virtual-host addressing the bucket's own host name is resolved and judged
   as well.
3. **Pinning.** The client is built with exactly the vetted addresses for the host and no resolver of its own, so a second lookup cannot
   return something else (DNS rebinding). SNI and the certificate name stay the host. A target is judged again every `repin_secs`; the
   same answer keeps the same client, a different one is vetted afresh and a bad one fails closed.
4. **Containment.** No redirects are followed (a redirect is the other way to reach an unchecked address), no proxy settings are read,
   connections and requests are bounded in time, and object bodies are bounded in size by the object store reader.

Refusals are `STORAGE_ENDPOINT_BLOCKED` (`INVALID_ARGUMENT`) with the reason in `paths[0]`: `not_https`, `credentials_in_url`, `port`,
`unresolvable`, `loopback`, `private`, `link_local`, `metadata`, `reserved`, `malformed`.

## How objects are routed

Every repository has a namespace (`ns/<hex>`). Without a target it is under the instance bucket's root. With one, it is under the
target's prefix: `<prefix>/ns/<hex>/...`. A repository's route (`repo_storage`) names its primary place, optionally a fallback (where
older objects still are) and, only while a migration copies, a mirror. The registry builds each repository's object store from the
route, and rebuilds it when the route changes, so no code outside `storage/` knows about targets.

* **Reads** try the primary, then the fallback, only on "not found". Other errors are not hidden.
* **Writes** go to the primary, and during a migration also to the mirror. Both must succeed.
* **Deletes** (garbage collection, drops) reach every place the object can be.
* **Lists** are the union of the places.

If a target is not usable (disabled, pending after an edit, failed, endpoint now blocked, vault locked), opening its repositories
answers `UNAVAILABLE` with the reason. Writes are never silently sent somewhere else: that would split a repository.

A fork starts in the instance bucket, whatever the source's route; a fork across buckets copies through the server.

## Migration

`StartMigration(repo, to_target_id, delete_source)`:

1. **Mirror.** The route gets `mirror_to`. From now on every write and delete goes to the source and the destination.
2. **Copy.** Every object under the namespace is copied in key order. The cursor and counts are persisted per chunk
   (`objects_total`, `objects_copied`, `bytes_copied`). An object already at the destination with the same size is skipped, so a restart
   or a second migration continues instead of starting over.
3. **Verify.** After `migration_settle_secs` (so every replica sees the mirror), each object is read from both places and the hashes
   are compared; a missing or different one is copied again and compared once more. A copy that still differs fails the migration.
4. **Switch.** One compare-and-swap on the route makes the destination the primary and clears mirror and fallback. Reads and writes
   never stopped.
5. **Clean up**, only with `delete_source`: after the settle time, the old objects are deleted. The destination and source must not be
   the same bucket and prefix (refused at the start).

A failure or a cancel before the switch clears the mirror and leaves the source authoritative and untouched, with a sanitised `error`.
`CancelMigration` is idempotent; after the switch it changes nothing. One migration per repository at a time (`MIGRATION_ACTIVE`).
Migrations hold a lease on their row and are picked up by any replica when the lease runs out, so a stopped server's migration resumes.

Progress is visible through `GetMigration` and `ListMigrations`, in the log (`storage migration` lines), in the audit events
(`storage.migration.started`, `storage.migration.finished`) and, in process, on `Engine::subscribe`.

## Health

Every `health_interval_secs` each enabled target gets the verification probe (without using the owner's allowance). Three failures in a
row mark it `FAILED` with a sanitised `last_error`, which makes its repositories answer `UNAVAILABLE` at once rather than time out; the
first success brings it back to `ENABLED`. A pass writes to the database only when something changed.

## Quotas and errors

| Limit | Value | Error |
|---|---|---|
| targets per owner | `max_targets_per_owner` (64) | `QUOTA_EXCEEDED` |
| repositories per target | `max_repos_per_target` (1000) | `QUOTA_EXCEEDED` |
| verifications per target | 6 per hour | `RATE_LIMITED` |
| migrations per repository | 1 active | `MIGRATION_ACTIVE` |

Provider errors reach users as one of: the provider rejected the credentials; access denied (check the key's permissions on the bucket);
the bucket or object was not found; the TLS connection failed; the request timed out; the provider is rate limiting or unavailable; the
provider's storage quota is exceeded; the endpoint could not be reached; the provider returned an unexpected response.

## Threat model

| Threat | Defence |
|---|---|
| SSRF: a target pointing at the server's network (databases, admin listener, metadata) | the guard: every resolved address judged by address, loopback and private ranges refused, mapped and translated forms included |
| DNS rebinding: a name that answers public once and private later | the client has the vetted addresses only; targets are judged again every `repin_secs`; a failed judgement fails closed |
| Redirect to an internal address | redirects are not followed |
| Proxy environment variables redirecting the traffic | the client reads no proxy settings |
| The operator's own cloud credentials used against a user's endpoint | the S3 client is built from explicit settings only: never the environment, never instance metadata |
| Cleartext credentials in the database | XChaCha20-Poly1305, bound to kind, owner and row; master key in a private file, rotatable |
| Credentials in logs, errors, audit, responses | write-only API; `Debug` prints `<redacted>`; provider text is never copied; tests plant a secret and search every output |
| One tenant using another's target | targets are scoped to one owner; an id that is not the caller's is `NOT_FOUND`; a repository-owned target serves only that repository |
| Probing which ids or owners exist | same answer for an unknown id and one the caller may not see |
| Hostile bucket serving huge or damaged objects | object bodies are size-capped; every read is verified against its content-addressed key; a mismatch is treated as corrupt |
| Hostile bucket deleting or withholding objects | not preventable by the server: the owner of the bucket controls its availability. The content hashes make tampering detectable, not the loss recoverable; keep the source (no `delete_source`) until satisfied |
| Brute use of verification as a port scanner | https and the port deny list, public addresses only, six verifications per target per hour, fixed error phrases |
| A migration deleting the data it copied | the same bucket and prefix are refused; deletion happens after the switch and the verified copy |
| Lost master key file | stored credentials cannot be opened; the owners re-enter them (`UpdateTarget` with new credentials). Back the file up |

What the server cannot defend against: the provider (or whoever holds the bucket credentials) reading the objects. Objects are
stored as the repository's own content; encrypt the bucket on the provider's side if that matters.

## Testing

`crates/asc-serve/src/storage/tests.rs` runs the whole feature against `fake_s3.rs`, an in-process S3 that checks SigV4 signatures with
the one key pair it knows, supports list v2, conditional put, copy and bulk delete, and injects faults and delays. It covers: every guard
rule through the service (and a DNS answer that turns private), each failing verification step, rate limits, scoping between users,
sealed credentials and rotation, secrets planted and searched for in logs/errors/audit/debug output, routing into the registry,
migration (happy path, delete source, failure and second try, resume after a stopped server, cancel, writes during the copy, repair of
a damaged copy, moving back), health and recovery. `netguard.rs` and `vault/` have their own unit tests, and
`crates/asc-store/src/s3.rs` tests the target settings. The compose `garage` service remains available for running the same flow against
a real S3 implementation with `allow_loopback_http`.
