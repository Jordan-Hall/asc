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
| `asc_requests_total` | counter | server | kind,class | Requests on the public listener; kind is grpc, mcp, ui, git or http, class is 2xx to 5xx or error (a failed RPC). |
| `asc_queue_depth` | gauge | server | queue | Work waiting: queue is outbox, agent_runs or workflow_runs. |
| `asc_workflow_runs_active` | gauge | server | state | Workflow runs not finished, by state (queued, running, waiting_approval). |
| `asc_workflow_runs_total` | counter | server | state | Finished workflow runs by state (succeeded, failed, cancelled). |
| `asc_agent_runs_active` | gauge | server | state | Agent runs not finished, by state (queued, running, waiting_input). |
| `asc_agent_runs_total` | counter | server | state | Finished agent runs by state (succeeded, failed, cancelled, budget_exceeded, expired). |
| `asc_storage_targets` | gauge | server | state | Bring-your-own storage targets by state. |
| `asc_storage_migrations` | gauge | server | state | Storage migrations by state. |
| `asc_client_retries_total` | counter | cli,daemon | - | Client RPC retries. |
| `asc_warm_lag_seconds` | gauge | warmer | - | Age of the trunk tip in the host cache. |
| `asc_warm_objects_total` | counter | warmer | - | Objects warmed. |
| `asc_warm_failures_total` | counter | warmer | - | Warm failures. |
| `host_asc_rpc_seconds` | histogram | host | method | Host-side asc RPC latency. |
| `host_asc_shadow_mismatch_total` | counter | host | method | Shadow-mode mismatches. |

## Server scrape

`asc serve` exposes these on the admin listener, never on the public one: `[admin] listen` (default `127.0.0.1:9464`) serves
`GET /metrics` (OpenMetrics text) and the detailed `/readyz`. A listener that is not on loopback is refused at startup unless
`[admin] token_file` is set; with a token file every admin route, loopback or not, needs `Authorization: Bearer <token>` (or Basic
with the token as the password). A scrape job:

```yaml
- job_name: asc
  authorization: { credentials_file: /etc/prometheus/asc-admin-token }
  static_configs: [{ targets: ["asc-admin.internal:9464"] }]
```

`asc_requests_total` and `asc_rpc_duration_seconds` (rpc is `<package.Service>/<Method>`, `other` beyond 512 distinct names; code is
`OK`, `grpc-<n>` or `http-<n>`) are counted as requests finish, per instance. The queue, run and storage series are read from the
database when the scrape arrives, so any instance reports the whole deployment (read them from one target, not summed over the
replicas). The `_total` series of runs are totals over the rows that still exist: a retention prune can lower them.

