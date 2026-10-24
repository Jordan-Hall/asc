# Runbook: code intelligence (server)

Metrics are listed in docs/metrics.md; alert rules are in deploy/alerts.yml (group `asc-codeintel`).
Search, symbol and semantic queries are served from the index; the `ci_jobs` queue feeds it.

## Normal ranges

- asc_ci_index_lag_seconds: under 60 in steady state; under 900 during a backfill.
- asc_ci_jobs{state="queued"}: drains to zero between bursts; {state="dead"} stays 0.
- asc_ci_query_seconds{rpc}: p99 under 0.5 for Search and FindSymbols, under 2 for SemanticSearch.
- asc_ci_candidates_ratio: under 0.05 for typical literal queries. A sustained high value means
  the trigram prefilter is not narrowing (very common terms or a missing index segment).
- asc_ci_coverage: 1.0 once the backfill finishes; below 0.95 for an hour means files are
  not being indexed.

## Alerts

- AscCodeIntelIndexLag: lag over 900 s for 15 m. Check the worker is running, then the queue.
- AscCodeIntelDeadJobs: dead jobs present for 30 m. Read `last_error` on those rows.
- AscCodeIntelQuerySlow: Search p99 over 2 s for 15 m.
- AscCodeIntelCoverage: coverage under 0.95 for 1 h.

## First diagnostic commands

    curl -s http://127.0.0.1:9464/metrics | grep '^asc_ci_'
    SELECT kind, state, count(*), min(created_at) FROM ci_jobs GROUP BY kind, state;
    SELECT kind, key, attempts, last_error FROM ci_jobs WHERE state = 'dead' LIMIT 20;

Jobs retry with backoff. Dead jobs are not retried until a newer pipeline version or a
backfill enqueues them again. Logs are JSON lines; filter on `tier=server` and the `rpc` field
(SPEC 16.1).
