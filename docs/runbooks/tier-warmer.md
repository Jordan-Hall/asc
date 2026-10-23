# Runbook: warmer

Metrics are listed in docs/metrics.md; alert rules are in deploy/alerts.yml.

## Normal ranges

asc_warm_lag_seconds under 3600; asc_warm_failures_total flat.

## First diagnostic command

    cat the node-exporter textfile directory's asc_warm.prom

Logs are JSON lines; filter on the `tier` and `event` fields (SPEC 16.1).
