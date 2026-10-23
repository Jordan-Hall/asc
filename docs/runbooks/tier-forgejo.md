# Runbook: Forgejo plugin

Metrics are listed in docs/metrics.md; alert rules are in deploy/alerts.yml.

## Normal ranges

forgejo_asc_rpc_seconds p99 under 2 s; forgejo_asc_shadow_mismatch_total flat.

## First diagnostic command

    check the Forgejo log for asc RPC errors

Logs are JSON lines; filter on the `tier` and `event` fields (SPEC 16.1).
