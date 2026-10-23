# Runbook: cache tiers

Metrics are listed in docs/metrics.md; alert rules are in deploy/alerts.yml.

## Normal ranges

hit ratio 0.99 or more at host and cache_role, 0.95 or more at server_mem and server_disk; three days below target raise AscCacheHitRateBelowSloThreeDays.

## First diagnostic command

    curl -s 127.0.0.1:9464/metrics | grep asc_cache_requests_total

Logs are JSON lines; filter on the `tier` and `event` fields (SPEC 16.1).
