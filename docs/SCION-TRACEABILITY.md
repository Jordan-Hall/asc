# Scion checklist traceability

This table is a copy of SPEC section 1.1. SPEC wins; a unit test (`help_git::traceability_rows`) checks that every checklist number 1 to 32 has a row in both.

| # | Item | asc handling | Where |
|---|---|---|---|
| 1 | Always online, plus local-only | Online by default; local-only mode; offline subset; history beyond the local window is answered by the server | §6.1, §6.3, §6.4 |
| 2 | Client-server asymmetry | Server does history, blame, merge, land, rebase; client keeps a bounded cache and a bounded history window | §4, §8, §3.5, §6.3 |
| 3 | API-first | Every client query is a public RPC; `asc api` | §7 |
| 4 | Horizontal scale, no packfiles, no maintenance | No packfiles; `gc`/`repack` are no-ops | §3.3, §11.2 |
| 5 | Cheap commits, cost scales with the change | A local commit costs one row; the daemon uploads it in the background (§21.3.1); server `Commit`/`Merge`/`Apply`; ingest O(changed paths × depth) | §8.7, §8.4, §21 |
| 6 | Few global refs | Shared refs are only `master`, release branches and tags; a pushed branch is one owner-only user ref; drafts are heads, not refs; agent refs expire and are not fetched by default; changesets need no ref | §2.7, §6.1, §8.7, §21.1, §21.5.1 |
| 7 | Optional, sparse, virtual working copy | Virtual and sparse workspaces; writes through the API | §10, §8.7 |
| 8 | Changeset is a WAL entry | Changes relative to the first parent; visible on ingest | §2.6, §8.7 |
| 9 | Accepting changesets needs no ref move | `Commit` without `update_ref`; `Ingest` RPC | §8.7 |
| 10 | One generic derived-data mechanism | `Derived` and `DerivedKeyed`; named future types | §5 |
| 11 | Content-addressed, immutable | BLAKE3 ids, create-only puts | §2.1, §3.3 |
| 12 | Want-only negotiation | Explicit wants; Log-driven history fetch; resumable | §6.3 |
| 13 | Sparse/virtual need no protocol support | The client does not ask | §6.3, §10 |
| 14 | Per-node daemon | Per-user daemon plus shared cache; CI host cache as the node store | §9.1 |
| 15 | Daemon boots from a seed | Seed snapshots published by the server | §12.3 |
| 16 | Caches stream stored bytes | `get_stored`; no per-client computation | §3.2 |
| 17 | Every tier speaks the same RPCs | Object tier contract; `asc serve --role cache` | §7 |
| 18 | Strict tiering | Retries with budget, coalescing, shedding, no tier bypass | §16.3 |
| 19 | Stateless tiers, > 99% hit rate | Cache SLOs; every cache is disposable | §16.4 |
| 20 | No client or cache maintenance | No packfiles, no projection ODB, LRU caches | §3.5, §9.4 |
| 21 | Metadata first | Milestone M-meta | §18 |
| 22 | Ingestion invariants, memory and speed | I1–I7 | §14.4 |
| 23 | No brute-force lookups | `land_map`, `FindLanding`, mainline index | §8.9, §5.1 |
| 24 | Hermetic simulation, fuzzing, rate limiting | `asc-sim`, fuzz targets, admission control | §17.2, §17.3, §16.3 |
| 25 | No over-engineering; no re-validation of trusted services | Trust boundaries | §15.4 |
| 26 | Repo ACLs plus directory permissions | Write rules in pre-receive; read isolation by composed repos | §15.5 |
| 27 | Semantic review constraints | `Classify` RPC; Forgejo relaxed approvals (K17) | §8.10 |
| 28 | Post-merge testing; agents resolve conflicts | `Rebase` RPC with structured conflicts; content-merge land; `green/*` refs | §8.11, §12.6 |
| 29 | Per-component monitoring | Per-tier metric and log catalogue; runbooks | §16.1 |
| 30 | Ancestry through indexes | Generation numbers, skip lists, mainline and path index | §4.3, §5, §5.1 |
| 31 | Standardised server RPCs | `asc.v1` public API, forge-neutral; `asc api` | §7 |
| 32 | Negotiation can change; sparse and shallow compose | No asc-side git helper (the full-projection decision, §0 principle 6; ASC's plugins use the asc API, §24). The Forgejo git bridge is served by real git, which handles `filter`/`shallow`/`deepen` | §13.1, §9.6 |
