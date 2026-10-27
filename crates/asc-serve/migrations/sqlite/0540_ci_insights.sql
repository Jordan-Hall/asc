-- CI failure grouping and flaky tests (docs/BUILDOUT.md section 8). Per namespace, like the check
-- tables. Every number is BIGINT so the sqlx Any driver reads one type on both databases.
--
-- One row per reported test case of a (run, job); a report for the same (run, job) replaces them.
-- `group_id` is the failure group of a failed case (0 for the rest). `change` and `version` are the
-- run's (empty and 0 for a run on the trunk); `head` is the code that was tested, empty when unknown.
CREATE TABLE test_cases (
    ns          BLOB   NOT NULL,
    run_id      BIGINT NOT NULL,
    job_id      BIGINT NOT NULL,
    seq         BIGINT NOT NULL,
    job_name    TEXT   NOT NULL,
    name        TEXT   NOT NULL,
    outcome     BIGINT NOT NULL,
    attempt     BIGINT NOT NULL,
    duration_ms BIGINT NOT NULL DEFAULT 0,
    group_id    BIGINT NOT NULL DEFAULT 0,
    message     TEXT   NOT NULL DEFAULT '',
    change      BLOB   NOT NULL,
    version     BIGINT NOT NULL DEFAULT 0,
    head        BLOB   NOT NULL,
    at          BIGINT NOT NULL,
    PRIMARY KEY (ns, run_id, job_id, seq)
);
CREATE INDEX test_cases_by_test ON test_cases (ns, job_name, name, at);
CREATE INDEX test_cases_by_group ON test_cases (ns, group_id, at);
CREATE INDEX test_cases_by_time ON test_cases (ns, at);

-- One group per signature: BLAKE3 over the job, the test and the normalised failure.
CREATE TABLE failure_groups (
    ns         BLOB   NOT NULL,
    id         BIGINT NOT NULL,
    signature  TEXT   NOT NULL,
    title      TEXT   NOT NULL,
    location   TEXT   NOT NULL DEFAULT '',
    test_name  TEXT   NOT NULL,
    job_name   TEXT   NOT NULL,
    state      BIGINT NOT NULL,
    note       TEXT   NOT NULL DEFAULT '',
    state_by   TEXT   NOT NULL DEFAULT '',
    state_at   BIGINT NOT NULL DEFAULT 0,
    first_seen BIGINT NOT NULL,
    last_seen  BIGINT NOT NULL,
    PRIMARY KEY (ns, id)
);
CREATE UNIQUE INDEX failure_groups_by_signature ON failure_groups (ns, signature);
CREATE INDEX failure_groups_by_seen ON failure_groups (ns, last_seen, id);
CREATE INDEX failure_groups_by_test ON failure_groups (ns, job_name, test_name);

-- Per day totals of one test in one job, for the history chart.
CREATE TABLE flaky_daily (
    ns       BLOB   NOT NULL,
    job_name TEXT   NOT NULL,
    test     TEXT   NOT NULL,
    day      BIGINT NOT NULL,
    runs     BIGINT NOT NULL,
    failures BIGINT NOT NULL,
    flips    BIGINT NOT NULL,
    PRIMARY KEY (ns, job_name, test, day)
);
CREATE INDEX flaky_daily_by_day ON flaky_daily (ns, day);

-- The verdict over the last runs of one test in one job.
CREATE TABLE flaky_tests (
    ns         BLOB   NOT NULL,
    job_name   TEXT   NOT NULL,
    test       TEXT   NOT NULL,
    runs       BIGINT NOT NULL,
    failures   BIGINT NOT NULL,
    flips      BIGINT NOT NULL,
    first_flip BIGINT NOT NULL DEFAULT 0,
    last_flip  BIGINT NOT NULL DEFAULT 0,
    flaky      BIGINT NOT NULL,
    updated    BIGINT NOT NULL,
    PRIMARY KEY (ns, job_name, test)
);
CREATE INDEX flaky_tests_by_flaky ON flaky_tests (ns, flaky, last_flip);
