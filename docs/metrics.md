# Metrics catalogue

SPEC section 16.1 fixes these names. `crates/asc/src/telemetry.rs` holds the same table and a test fails when the two differ. Server and cache role expose `/metrics` on the admin listener; the daemon on `--metrics-listen`; the warmer writes the node-exporter textfile directory.

| Name | Type | Tier | Labels | Meaning |
|---|---|---|---|---|
| `asc_rpc_duration_seconds` | histogram | server,cache | rpc,code | RPC latency. |
| `asc_store_op_seconds` | histogram | server,cache | op,backend | Object-store operation latency. |
| `asc_ref_cas_conflicts_total` | counter | server,cache | - | Ref compare-and-swap conflicts. |
| `asc_land_total` | counter | server,cache | outcome | Land attempts by outcome. |
| `asc_hook_seconds` | histogram | server,cache | hook,outcome | Hook run time. |
| `asc_outbox_lag_seconds` | gauge | server,cache | - | Age of the oldest pending outbox entry. |
| `asc_outbox_failures_total` | counter | server,cache | - | Outbox delivery failures. |
| `asc_mirror_import_lag_seconds` | gauge | server,cache | - | Age of the oldest unimported mirror push. |
| `asc_derive_lag_changesets` | gauge | server,cache | - | Changesets awaiting derivation. |
| `asc_changeset_reads` | histogram | server,cache | rpc | Changesets read per RPC. |
| `asc_graph_walk_nodes` | histogram | server,cache | - | Graph nodes visited per walk. |
| `asc_cache_requests_total` | counter | server,cache,daemon | tier,result | Cache lookups; tier is client, host, job, cache_role, server_mem or server_disk; result is hit or miss. |
| `asc_rate_limited_total` | counter | server,cache | class | Requests refused by rate limits. |
| `asc_inflight_requests` | gauge | server,cache | - | Requests in flight. |
| `asc_shed_total` | counter | server,cache | reason | Requests shed by admission control. |
| `asc_coalesced_total` | counter | server,cache | - | Requests coalesced into one. |
| `asc_turbo_hit_ratio` | gauge | server,cache | scope | Turbo index hit ratio. |
| `asc_rss_bytes` | gauge | server,cache,daemon | - | Resident set size. |
| `asc_daemon_hydrations_total` | counter | daemon | process | Hydrations by requesting process. |
| `asc_daemon_hydration_seconds` | histogram | daemon | - | Hydration latency. |
| `asc_daemon_fetch_bytes_total` | counter | daemon | - | Bytes fetched. |
| `asc_daemon_cache_bytes` | gauge | daemon | dir | Cache directory size. |
| `asc_daemon_journal_entries` | gauge | daemon | - | Journal entries. |
| `asc_daemon_pending_paths` | gauge | daemon | - | Paths awaiting reconcile. |
| `asc_daemon_reconcile_paths_total` | counter | daemon | - | Paths reconciled. |
| `asc_daemon_provider_errors_total` | counter | daemon | op | Provider errors by operation. |
| `asc_daemon_uptime_seconds` | gauge | daemon | - | Daemon uptime. |
| `asc_ci_index_lag_seconds` | gauge | server | - | Code-intelligence index lag. |
| `asc_ci_jobs` | gauge | server | kind,state | Code-intelligence jobs. |
| `asc_ci_query_seconds` | histogram | server | rpc | Code-intelligence query latency. |
| `asc_ci_candidates_ratio` | gauge | server | - | Candidate ratio of code-intelligence queries. |
| `asc_ci_coverage` | gauge | server | - | Indexed share of the tree. |
| `asc_client_retries_total` | counter | cli,daemon | - | Client RPC retries. |
| `asc_warm_lag_seconds` | gauge | warmer | - | Age of the trunk tip in the host cache. |
| `asc_warm_objects_total` | counter | warmer | - | Objects warmed. |
| `asc_warm_failures_total` | counter | warmer | - | Warm failures. |
| `host_asc_rpc_seconds` | histogram | host | method | Host-side asc RPC latency. |
| `host_asc_shadow_mismatch_total` | counter | host | method | Shadow-mode mismatches. |
