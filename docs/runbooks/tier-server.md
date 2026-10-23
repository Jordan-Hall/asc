# Runbook: server and cache role

Metrics are listed in docs/metrics.md; alert rules are in deploy/alerts.yml.

## Normal ranges

asc_rpc_duration_seconds p99 under 1 s; asc_outbox_lag_seconds under 60; asc_shed_total flat at 0.

## First diagnostic command

    curl -s -H "Authorization: Bearer $TOKEN" 127.0.0.1:9464/readyz

Logs are JSON lines; filter on the `tier` and `event` fields (SPEC 16.1).
