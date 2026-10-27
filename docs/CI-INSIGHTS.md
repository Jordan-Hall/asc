# CI failure groups, flaky tests and quarantine

A runner (or the forge adapter) reports the test cases of a job with `CiInsights.ReportTestCases`. The server groups the
failures by root cause, says which failures are new against the trunk, finds flaky tests from their recent results and lets
a maintainer quarantine a known failure so it does not block a landing.

Feature string: `ci-insights.v1`. Wire contract: `proto/asc/v1/ciinsights.proto`. Pure logic (normalisation, signatures,
flaky detection, quarantine rules) is in `crates/asc-ci/src/insights/`; storage and the service are in
`crates/asc-serve/src/ci/insights/`. The design is in `docs/BUILDOUT.md`, section 8.

## Grouping by root cause

Runners print one failure differently each time. The message is first masked (the built-in secret rules and the repository's
own), then normalised: ANSI codes, uuids, timestamps, durations, IP addresses and ports, hex ids, absolute paths and numbers
are replaced by placeholders, and the location of the failure (the first source position the message names: a Rust panic, a
Go `file_test.go:12:`, a Python `File "x", line 3`, a JavaScript or Java frame, any `path.ext:line`) is kept without its line
number, so an edit above the failing line does not start a new group. The signature is BLAKE3 over the job, the test, the
location and the first normalised lines, and one group exists per signature. The title of a group is its first meaningful
line. Stored messages are cut to 8 KiB. A replayed report of the same run and job replaces the earlier one.

A group is **new** when none of its occurrences came from a run on the trunk. A result is **stale** when its change has a
newer version than the one the run was made for; stale results still show, marked, and `CiRun.stale` says the same for a
whole run (`Reviews.ListRuns`).

## Flaky tests

A test is flaky when the same code gives it both verdicts. Two results belong to the same code when they were made for the
same head (`ReportTestCasesRequest.head`) or by retries inside one run. The detector counts a **flip** each time the verdict
changes between two consecutive results of one head. A test that failed on a new head after it passed on the old one did not
flip: the code changed. The rate is flips over runs in the window (the last `[ci_insights] flaky_window_runs`, default 100);
a test is flaky when it flipped at least once and the rate is at or above the policy `[ci] flaky_threshold` (default 0.02).
`FlakyHistory` returns per-day runs, failures and flips; `ListFlakyTests` the flaky list with flip counts.

## Quarantine

`SetFailureGroupState` moves a group between `OPEN`, `QUARANTINED` and `RESOLVED`. Quarantine needs the repository policy to
allow it (`[ci] allow_quarantine = true`; otherwise `FAILED_PRECONDITION`) and a maintainer (otherwise `PERMISSION_DENIED`).
It quarantines one **signature**: a different failure of the same test still blocks. A landing check reads the list with
`store::quarantine` and asks `store::blocking_failures(run)` for what still blocks: the failed or errored cases of the run
whose group is not quarantined and that did not pass on a later attempt. Every state change is audited
(`ci.failure_group.state`) and is idempotent per `x-asc-op-id`.

```toml
# repository policy, .asc/policy.toml
[ci]
flaky_threshold = 0.02
allow_quarantine = true

# server
[ci_insights]
retention_days = 90         # test cases; 0 keeps them all. Per-day history is kept for 180 days.
flaky_window_runs = 100
```

Bounds: 5,000 cases per report, 16 MiB per report, test names 512 bytes, notes 1 KiB.
