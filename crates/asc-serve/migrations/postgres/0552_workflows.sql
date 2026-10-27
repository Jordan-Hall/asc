-- Workflows (docs/BUILDOUT.md section 13, docs/WORKFLOWS.md). A definition lives in the repository
-- (`.asc/workflows/<name>.toml` on trunk); the server keeps only whether it is enabled, the runs, and the
-- secrets its command steps read. `state` of a run is the wire WorkflowRunState (1 queued, 2 running,
-- 3 waiting approval, 4 succeeded, 5 failed, 6 cancelled); `state` of a step is the wire WorkflowStepState
-- (1 pending, 2 running, 3 waiting, 4 succeeded, 5 failed, 6 skipped, 7 cancelled). A run keeps the text of
-- the definition it was started from (`definition`), so a later change to the file does not touch it.
-- `owner` and `heartbeat` are the lease of the process that drives a running run; a stale heartbeat means
-- that process died and another takes the run over.
CREATE TABLE workflow_state (
    repo       TEXT   NOT NULL,
    name       TEXT   NOT NULL,
    enabled    BIGINT NOT NULL DEFAULT 0,
    updated_by TEXT   NOT NULL DEFAULT '',
    updated    BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (repo, name)
);

CREATE TABLE workflow_runs (
    id               BIGINT NOT NULL PRIMARY KEY,
    repo             TEXT   NOT NULL,
    workflow         TEXT   NOT NULL,
    source           BYTEA NOT NULL,
    definition       TEXT   NOT NULL,
    state            BIGINT NOT NULL,
    trigger_kind     BIGINT NOT NULL,
    started_by       TEXT   NOT NULL DEFAULT '',
    requester        TEXT   NOT NULL DEFAULT '',
    change           BYTEA NOT NULL,
    change_cs        BYTEA NOT NULL,
    event_json       TEXT   NOT NULL DEFAULT '{}',
    inputs_json      TEXT   NOT NULL DEFAULT '{}',
    depth            BIGINT NOT NULL DEFAULT 0,
    origin           TEXT   NOT NULL DEFAULT '',
    created          BIGINT NOT NULL,
    started          BIGINT NOT NULL DEFAULT 0,
    finished         BIGINT NOT NULL DEFAULT 0,
    error            TEXT   NOT NULL DEFAULT '',
    cancel_requested BIGINT NOT NULL DEFAULT 0,
    owner            TEXT   NOT NULL DEFAULT '',
    heartbeat        BIGINT NOT NULL DEFAULT 0
);
CREATE INDEX workflow_runs_by_repo ON workflow_runs (repo, id);
CREATE INDEX workflow_runs_by_workflow ON workflow_runs (repo, workflow, id);
CREATE INDEX workflow_runs_by_state ON workflow_runs (state, id);

CREATE TABLE workflow_steps (
    run_id       BIGINT NOT NULL,
    step         TEXT   NOT NULL,
    ord          BIGINT NOT NULL,
    state        BIGINT NOT NULL,
    agent_run_id BIGINT NOT NULL DEFAULT 0,
    started      BIGINT NOT NULL DEFAULT 0,
    finished     BIGINT NOT NULL DEFAULT 0,
    decided_by   TEXT   NOT NULL DEFAULT '',
    comment      TEXT   NOT NULL DEFAULT '',
    error        TEXT   NOT NULL DEFAULT '',
    exit_code    BIGINT NOT NULL DEFAULT 0,
    timed_out    BIGINT NOT NULL DEFAULT 0,
    output       TEXT   NOT NULL DEFAULT '',
    ci_run_id    BIGINT NOT NULL DEFAULT 0,
    usage_json   TEXT   NOT NULL DEFAULT '',
    PRIMARY KEY (run_id, step)
);

CREATE TABLE workflow_events (
    run_id   BIGINT NOT NULL,
    seq      BIGINT NOT NULL,
    time_ms  BIGINT NOT NULL,
    step     TEXT   NOT NULL DEFAULT '',
    snapshot BYTEA NOT NULL,
    PRIMARY KEY (run_id, seq)
);

-- One row per (workflow, event): the event fires the workflow at most once, however many servers
-- look at the same event. Schedule slots are `cron:<unix minute>`.
CREATE TABLE workflow_slots (
    repo TEXT   NOT NULL,
    name TEXT   NOT NULL,
    slot TEXT   NOT NULL,
    at   BIGINT NOT NULL,
    PRIMARY KEY (repo, name, slot)
);
CREATE INDEX workflow_slots_by_time ON workflow_slots (at);

-- How far the event sources of a repository have been read (`refs`: ref log sequence, `checks`: time,
-- `comments`: comment id, `cron`: unix minute).
CREATE TABLE workflow_cursors (
    repo   TEXT   NOT NULL,
    source TEXT   NOT NULL,
    value  BIGINT NOT NULL,
    PRIMARY KEY (repo, source)
);

-- Secrets a command step may read, sealed by the vault under the kind `workflow-secret` and bound to
-- the repository and the name. `hint` is the last four characters of a long value, else empty.
CREATE TABLE workflow_secrets (
    repo       TEXT   NOT NULL,
    name       TEXT   NOT NULL,
    sealed     BYTEA NOT NULL,
    key_gen    BIGINT NOT NULL,
    hint       TEXT   NOT NULL DEFAULT '',
    created    BIGINT NOT NULL,
    updated    BIGINT NOT NULL,
    updated_by TEXT   NOT NULL DEFAULT '',
    PRIMARY KEY (repo, name)
);
CREATE INDEX workflow_secrets_by_gen ON workflow_secrets (key_gen);
