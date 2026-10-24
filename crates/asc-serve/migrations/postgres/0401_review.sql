-- Review threads, review requests, inbox state and check runs (PLAN: host-neutral Inbox, Review and Ci
-- services). All of it is per namespace and host-neutral: a forge adapter may fill the check tables or
-- mirror threads, but nothing here is a forge object. Every number is BIGINT so the sqlx Any driver reads
-- the same type on both databases. Ids are allocated per namespace as max + 1 inside the writing
-- transaction; a lost race is a primary-key conflict and the writer retries.
CREATE TABLE review_threads (
    ns          BYTEA   NOT NULL,
    id          BIGINT NOT NULL,
    change      BYTEA   NOT NULL,
    path        BYTEA   NOT NULL,
    version     BIGINT NOT NULL,
    side        BIGINT NOT NULL,
    start_line  BIGINT NOT NULL,
    end_line    BIGINT NOT NULL,
    status      BIGINT NOT NULL,
    opened_by   TEXT   NOT NULL,
    created     BIGINT NOT NULL,
    updated     BIGINT NOT NULL,
    resolved_by TEXT   NOT NULL DEFAULT '',
    resolved_at BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (ns, id)
);
CREATE INDEX review_threads_by_change ON review_threads (ns, change, id);
CREATE INDEX review_threads_by_opener ON review_threads (ns, opened_by, status);

CREATE TABLE review_comments (
    ns      BYTEA   NOT NULL,
    id      BIGINT NOT NULL,
    thread  BIGINT NOT NULL,
    author  TEXT   NOT NULL,
    body    TEXT   NOT NULL,
    created BIGINT NOT NULL,
    PRIMARY KEY (ns, id)
);
CREATE INDEX review_comments_by_thread ON review_comments (ns, thread, id);

-- A pending request has done_at = 0; the reviewer's first comment on the change finishes it.
CREATE TABLE review_requests (
    ns           BYTEA   NOT NULL,
    change       BYTEA   NOT NULL,
    reviewer     TEXT   NOT NULL,
    requested_by TEXT   NOT NULL,
    requested_at BIGINT NOT NULL,
    done_at      BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (ns, change, reviewer)
);
CREATE INDEX review_requests_by_reviewer ON review_requests (ns, reviewer, done_at);

-- Per-user pin and snooze. A row with no pin and no snooze is deleted.
CREATE TABLE inbox_state (
    ns            BYTEA   NOT NULL,
    handle        TEXT   NOT NULL,
    change        BYTEA   NOT NULL,
    pinned        BIGINT NOT NULL DEFAULT 0,
    snoozed_until BIGINT NOT NULL DEFAULT 0,
    updated       BIGINT NOT NULL,
    PRIMARY KEY (ns, handle, change)
);

-- Runs a runner or forge adapter reported against a change (`ci.proto`). (source, external_id) names a
-- run; `change` is empty for a run that belongs to no change.
CREATE TABLE check_runs (
    ns          BYTEA   NOT NULL,
    id          BIGINT NOT NULL,
    source      TEXT   NOT NULL,
    external_id TEXT   NOT NULL,
    name        TEXT   NOT NULL,
    change      BYTEA   NOT NULL,
    version     BIGINT NOT NULL,
    status      BIGINT NOT NULL,
    created     BIGINT NOT NULL,
    started     BIGINT NOT NULL DEFAULT 0,
    finished    BIGINT NOT NULL DEFAULT 0,
    url         TEXT   NOT NULL DEFAULT '',
    PRIMARY KEY (ns, id)
);
CREATE UNIQUE INDEX check_runs_by_external ON check_runs (ns, source, external_id);
CREATE INDEX check_runs_by_change ON check_runs (ns, change, id);

CREATE TABLE check_jobs (
    ns       BYTEA   NOT NULL,
    id       BIGINT NOT NULL,
    run      BIGINT NOT NULL,
    name     TEXT   NOT NULL,
    status   BIGINT NOT NULL,
    started  BIGINT NOT NULL DEFAULT 0,
    finished BIGINT NOT NULL DEFAULT 0,
    url      TEXT   NOT NULL DEFAULT '',
    log_size BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (ns, id)
);
CREATE UNIQUE INDEX check_jobs_by_name ON check_jobs (ns, run, name);

-- A job's log in chunks; `at_offset` is where the chunk starts in the whole log.
CREATE TABLE check_job_logs (
    ns        BYTEA   NOT NULL,
    job       BIGINT NOT NULL,
    at_offset BIGINT NOT NULL,
    data      BYTEA   NOT NULL,
    PRIMARY KEY (ns, job, at_offset)
);
