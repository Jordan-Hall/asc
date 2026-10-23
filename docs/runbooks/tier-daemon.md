# Runbook: daemon

Metrics are listed in docs/metrics.md; alert rules are in deploy/alerts.yml.

## Normal ranges

idle asc_rss_bytes under 64 MiB; asc_daemon_pending_paths near 0 when idle; asc_daemon_provider_errors_total flat.

## First diagnostic command

    asc daemon status --json

Logs are JSON lines; filter on the `tier` and `event` fields (SPEC 16.1).
